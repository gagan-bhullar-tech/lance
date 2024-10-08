// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use std::sync::Arc;

use arrow::array::ArrayData;
use arrow::datatypes::{
    ArrowPrimitiveType, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow::util::bit_util::ceil;
use arrow_array::{cast::AsArray, Array, ArrayRef, PrimitiveArray};
use arrow_schema::DataType;
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt};
use log::trace;
use num_traits::{AsPrimitive, PrimInt, ToPrimitive};
use snafu::{location, Location};

use lance_arrow::DataTypeExt;
use lance_core::{Error, Result};

use crate::buffer::LanceBuffer;
use crate::data::{DataBlock, FixedWidthDataBlock};
use crate::encoder::{BitpackingBufferMeta, EncodedBufferMeta};
use crate::{
    decoder::{PageScheduler, PrimitivePageDecoder},
    encoder::{BufferEncoder, EncodedBuffer},
};

#[derive(Debug)]
pub struct BitpackParams {
    pub num_bits: u64,

    pub signed: bool,
}

// Compute the number of bits to use for each item, if this array can be encoded using
// bitpacking encoding. Returns `None` if the type or array data is not supported.
pub fn bitpack_params(arr: ArrayRef) -> Option<BitpackParams> {
    match arr.data_type() {
        DataType::UInt8 => bitpack_params_for_type::<UInt8Type>(arr.as_primitive()),
        DataType::UInt16 => bitpack_params_for_type::<UInt16Type>(arr.as_primitive()),
        DataType::UInt32 => bitpack_params_for_type::<UInt32Type>(arr.as_primitive()),
        DataType::UInt64 => bitpack_params_for_type::<UInt64Type>(arr.as_primitive()),
        DataType::Int8 => bitpack_params_for_signed_type::<Int8Type>(arr.as_primitive()),
        DataType::Int16 => bitpack_params_for_signed_type::<Int16Type>(arr.as_primitive()),
        DataType::Int32 => bitpack_params_for_signed_type::<Int32Type>(arr.as_primitive()),
        DataType::Int64 => bitpack_params_for_signed_type::<Int64Type>(arr.as_primitive()),
        // TODO -- eventually we could support temporal types as well
        _ => None,
    }
}

// Compute the number bits to to use for bitpacking generically.
// returns None if the array is empty or all nulls
fn bitpack_params_for_type<T>(arr: &PrimitiveArray<T>) -> Option<BitpackParams>
where
    T: ArrowPrimitiveType,
    T::Native: PrimInt + AsPrimitive<u64>,
{
    let max = arrow::compute::bit_or(arr);
    let num_bits =
        max.map(|max| arr.data_type().byte_width() as u64 * 8 - max.leading_zeros() as u64);

    // we can't bitpack into 0 bits, so the minimum is 1
    num_bits
        .map(|num_bits| num_bits.max(1))
        .map(|bits| BitpackParams {
            num_bits: bits,
            signed: false,
        })
}

/// determine the minimum number of bits that can be used to represent
/// an array of signed values. It includes all the significant bits for
/// the value + plus 1 bit to represent the sign. If there are no negative values
/// then it will not add a signed bit
fn bitpack_params_for_signed_type<T>(arr: &PrimitiveArray<T>) -> Option<BitpackParams>
where
    T: ArrowPrimitiveType,
    T::Native: PrimInt + AsPrimitive<i64>,
{
    let mut add_signed_bit = false;
    let mut min_leading_bits: Option<u64> = None;
    for val in arr.iter() {
        if val.is_none() {
            continue;
        }
        let val = val.unwrap();
        if min_leading_bits.is_none() {
            min_leading_bits = Some(u64::MAX);
        }

        if val.to_i64().unwrap() < 0i64 {
            min_leading_bits = min_leading_bits.map(|bits| bits.min(val.leading_ones() as u64));
            add_signed_bit = true;
        } else {
            min_leading_bits = min_leading_bits.map(|bits| bits.min(val.leading_zeros() as u64));
        }
    }

    let mut min_leading_bits = arr.data_type().byte_width() as u64 * 8 - min_leading_bits?;
    if add_signed_bit {
        // Need extra sign bit
        min_leading_bits += 1;
    }
    // cannot bitpack into <1 bit
    let num_bits = min_leading_bits.max(1);
    Some(BitpackParams {
        num_bits,
        signed: add_signed_bit,
    })
}
#[derive(Debug)]
pub struct BitpackingBufferEncoder {
    num_bits: u64,
    signed_type: bool,
}

impl BitpackingBufferEncoder {
    pub fn new(num_bits: u64, signed_type: bool) -> Self {
        Self {
            num_bits,
            signed_type,
        }
    }
}

impl BufferEncoder for BitpackingBufferEncoder {
    fn encode(&self, arrays: &[ArrayRef]) -> Result<(EncodedBuffer, EncodedBufferMeta)> {
        // calculate the total number of bytes we need to allocate for the destination.
        // this will be the number of items in the source array times the number of bits.
        let count_items = arrays.iter().map(|arr| arr.len()).sum::<usize>();
        let dst_bytes_total = ceil(count_items * self.num_bits as usize, 8);

        let mut dst_buffer = vec![0u8; dst_bytes_total];
        let mut dst_idx = 0;
        let mut dst_offset = 0;
        for arr in arrays {
            pack_array(
                arr.clone(),
                self.num_bits,
                &mut dst_buffer,
                &mut dst_idx,
                &mut dst_offset,
            )?;
        }

        let data_type = arrays[0].data_type();
        Ok((
            EncodedBuffer {
                parts: vec![dst_buffer.into()],
            },
            EncodedBufferMeta {
                bits_per_value: (data_type.byte_width() * 8) as u64,
                bitpacking: Some(BitpackingBufferMeta {
                    bits_per_value: self.num_bits,
                    signed: self.signed_type,
                }),
                compression_scheme: None,
            },
        ))
    }
}

fn pack_array(
    arr: ArrayRef,
    num_bits: u64,
    dst: &mut [u8],
    dst_idx: &mut usize,
    dst_offset: &mut u8,
) -> Result<()> {
    match arr.data_type() {
        DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64 => {
            pack_buffers(
                arr.to_data(),
                num_bits,
                arr.data_type().byte_width(),
                dst,
                dst_idx,
                dst_offset,
            );

            Ok(())
        }
        _ => Err(Error::InvalidInput {
            source: format!("Invalid data type for bitpacking: {}", arr.data_type()).into(),
            location: location!(),
        }),
    }
}

fn pack_buffers(
    data: ArrayData,
    num_bits: u64,
    byte_len: usize,
    dst: &mut [u8],
    dst_idx: &mut usize,
    dst_offset: &mut u8,
) {
    let buffers = data.buffers();
    debug_assert_eq!(buffers.len(), 1);
    for buffer in buffers {
        pack_bits(buffer, num_bits, byte_len, dst, dst_idx, dst_offset);
    }
}

fn pack_bits(
    src: &[u8],
    num_bits: u64,
    byte_len: usize,
    dst: &mut [u8],
    dst_idx: &mut usize,
    dst_offset: &mut u8,
) {
    let bit_len = byte_len as u64 * 8;

    let mask = u64::MAX >> (64 - num_bits);

    let mut src_idx = 0;
    while src_idx < src.len() {
        let mut curr_mask = mask;
        let mut curr_src = src[src_idx] & curr_mask as u8;
        let mut src_offset = 0;
        let mut src_bits_written = 0;

        while src_bits_written < num_bits {
            dst[*dst_idx] += (curr_src >> src_offset) << *dst_offset as u64;
            let bits_written = (num_bits - src_bits_written)
                .min(8 - src_offset)
                .min(8 - *dst_offset as u64);
            src_bits_written += bits_written;
            *dst_offset += bits_written as u8;
            src_offset += bits_written;

            if *dst_offset == 8 {
                *dst_idx += 1;
                *dst_offset = 0;
            }

            if src_offset == 8 {
                src_idx += 1;
                src_offset = 0;
                curr_mask >>= 8;
                if src_idx == src.len() {
                    break;
                }
                curr_src = src[src_idx] & curr_mask as u8;
            }
        }

        // advance source_offset to the next byte if we're not at the end..
        // note that we don't need to do this if we wrote the full number of bits
        // because source index would have been advanced by the inner loop above
        if bit_len != num_bits {
            let partial_bytes_written = ceil(num_bits as usize, 8);

            // we also want to the next location in src, unless we wrote something
            // byte-aligned in which case the logic above would have already advanced
            let mut to_next_byte = 1;
            if num_bits % 8 == 0 {
                to_next_byte = 0;
            }

            src_idx += byte_len - partial_bytes_written + to_next_byte;
        }
    }
}

// A physical scheduler for bitpacked buffers
#[derive(Debug, Clone, Copy)]
pub struct BitpackedScheduler {
    bits_per_value: u64,
    uncompressed_bits_per_value: u64,
    buffer_offset: u64,
    signed: bool,
}

impl BitpackedScheduler {
    pub fn new(
        bits_per_value: u64,
        uncompressed_bits_per_value: u64,
        buffer_offset: u64,
        signed: bool,
    ) -> Self {
        Self {
            bits_per_value,
            uncompressed_bits_per_value,
            buffer_offset,
            signed,
        }
    }
}

impl PageScheduler for BitpackedScheduler {
    fn schedule_ranges(
        &self,
        ranges: &[std::ops::Range<u64>],
        scheduler: &Arc<dyn crate::EncodingsIo>,
        top_level_row: u64,
    ) -> BoxFuture<'static, Result<Box<dyn PrimitivePageDecoder>>> {
        let mut min = u64::MAX;
        let mut max = 0;

        let mut buffer_bit_start_offsets: Vec<u8> = vec![];
        let mut buffer_bit_end_offsets: Vec<Option<u8>> = vec![];
        let byte_ranges = ranges
            .iter()
            .map(|range| {
                let start_byte_offset = range.start * self.bits_per_value / 8;
                let mut end_byte_offset = range.end * self.bits_per_value / 8;
                if range.end * self.bits_per_value % 8 != 0 {
                    // If the end of the range is not byte-aligned, we need to read one more byte
                    end_byte_offset += 1;

                    let end_bit_offset = range.end * self.bits_per_value % 8;
                    buffer_bit_end_offsets.push(Some(end_bit_offset as u8));
                } else {
                    buffer_bit_end_offsets.push(None);
                }

                let start_bit_offset = range.start * self.bits_per_value % 8;
                buffer_bit_start_offsets.push(start_bit_offset as u8);

                let start = self.buffer_offset + start_byte_offset;
                let end = self.buffer_offset + end_byte_offset;
                min = min.min(start);
                max = max.max(end);

                start..end
            })
            .collect::<Vec<_>>();

        trace!(
            "Scheduling I/O for {} ranges spread across byte range {}..{}",
            byte_ranges.len(),
            min,
            max
        );

        let bytes = scheduler.submit_request(byte_ranges, top_level_row);

        let bits_per_value = self.bits_per_value;
        let uncompressed_bits_per_value = self.uncompressed_bits_per_value;
        let signed = self.signed;
        async move {
            let bytes = bytes.await?;
            Ok(Box::new(BitpackedPageDecoder {
                buffer_bit_start_offsets,
                buffer_bit_end_offsets,
                bits_per_value,
                uncompressed_bits_per_value,
                signed,
                data: bytes,
            }) as Box<dyn PrimitivePageDecoder>)
        }
        .boxed()
    }
}

#[derive(Debug)]
struct BitpackedPageDecoder {
    // bit offsets of the first value within each buffer
    buffer_bit_start_offsets: Vec<u8>,

    // bit offsets of the last value within each buffer. e.g. if there was a buffer
    // with 2 values, packed into 5 bits, this would be [Some(3)], indicating that
    // the bits from the 3rd->8th bit in the last byte shouldn't be decoded.
    buffer_bit_end_offsets: Vec<Option<u8>>,

    // the number of bits used to represent a compressed value. E.g. if the max value
    // in the page was 7 (0b111), then this will be 3
    bits_per_value: u64,

    // number of bits in the uncompressed value. E.g. this will be 32 for u32
    uncompressed_bits_per_value: u64,

    // whether or not to use the msb as a sign bit during decoding
    signed: bool,

    data: Vec<Bytes>,
}

impl PrimitivePageDecoder for BitpackedPageDecoder {
    fn decode(&self, rows_to_skip: u64, num_rows: u64) -> Result<DataBlock> {
        let num_bytes = self.uncompressed_bits_per_value / 8 * num_rows;
        let mut dest = vec![0; num_bytes as usize];

        // current maximum supported bits per value = 64
        debug_assert!(self.bits_per_value <= 64);

        let mut rows_to_skip = rows_to_skip;
        let mut rows_taken = 0;
        let byte_len = self.uncompressed_bits_per_value / 8;
        let mut dst_idx = 0; // index for current byte being written to destination buffer

        // create bit mask for source bits
        let mask = u64::MAX >> (64 - self.bits_per_value);

        for i in 0..self.data.len() {
            let src = &self.data[i];
            let (mut src_idx, mut src_offset) = match compute_start_offset(
                rows_to_skip,
                src.len(),
                self.bits_per_value,
                self.buffer_bit_start_offsets[i],
                self.buffer_bit_end_offsets[i],
            ) {
                StartOffset::SkipFull(rows_to_skip_here) => {
                    rows_to_skip -= rows_to_skip_here;
                    continue;
                }
                StartOffset::SkipSome(buffer_start_offset) => (
                    buffer_start_offset.index,
                    buffer_start_offset.bit_offset as u64,
                ),
            };

            while src_idx < src.len() && rows_taken < num_rows {
                rows_taken += 1;
                let mut curr_mask = mask; // copy mask

                // current source byte being written to destination
                let mut curr_src = src[src_idx] & (curr_mask << src_offset) as u8;

                // how many bits from the current source value have been written to destination
                let mut src_bits_written = 0;

                // the offset within the current destination byte to write to
                let mut dst_offset = 0;

                let is_negative = is_encoded_item_negative(
                    src,
                    src_idx,
                    src_offset,
                    self.bits_per_value as usize,
                );

                while src_bits_written < self.bits_per_value {
                    // write bits from current source byte into destination
                    dest[dst_idx] += (curr_src >> src_offset) << dst_offset;
                    let bits_written = (self.bits_per_value - src_bits_written)
                        .min(8 - src_offset)
                        .min(8 - dst_offset);
                    src_bits_written += bits_written;
                    dst_offset += bits_written;
                    src_offset += bits_written;
                    curr_mask >>= bits_written;

                    if dst_offset == 8 {
                        dst_idx += 1;
                        dst_offset = 0;
                    }

                    if src_offset == 8 {
                        src_idx += 1;
                        src_offset = 0;
                        if src_idx == src.len() {
                            break;
                        }
                        curr_src = src[src_idx] & curr_mask as u8;
                    }
                }

                // if the type is signed, need to pad out the rest of the byte with 1s
                let mut negative_padded_current_byte = false;
                if self.signed && is_negative && dst_offset > 0 {
                    negative_padded_current_byte = true;
                    while dst_offset < 8 {
                        dest[dst_idx] |= 1 << dst_offset;
                        dst_offset += 1;
                    }
                }

                // advance destination offset to the next location
                // note that we don't need to do this if we wrote the full number of bits
                // because source index would have been advanced by the inner loop above
                if self.uncompressed_bits_per_value != self.bits_per_value {
                    let partial_bytes_written = ceil(self.bits_per_value as usize, 8);

                    // we also want to move one location to the next location in destination,
                    // unless we wrote something byte-aligned in which case the logic above
                    // would have already advanced dst_idx
                    let mut to_next_byte = 1;
                    if self.bits_per_value % 8 == 0 {
                        to_next_byte = 0;
                    }
                    let next_dst_idx =
                        dst_idx + byte_len as usize - partial_bytes_written + to_next_byte;

                    // pad remaining bytes with 1 for negative signed numbers
                    if self.signed && is_negative {
                        if !negative_padded_current_byte {
                            dest[dst_idx] = 0xFF;
                        }
                        for i in dest.iter_mut().take(next_dst_idx).skip(dst_idx + 1) {
                            *i = 0xFF;
                        }
                    }

                    dst_idx = next_dst_idx;
                }

                // If we've reached the last byte, there may be some extra bits from the
                // next value outside the range. We don't want to be taking those.
                if let Some(buffer_bit_end_offset) = self.buffer_bit_end_offsets[i] {
                    if src_idx == src.len() - 1 && src_offset >= buffer_bit_end_offset as u64 {
                        break;
                    }
                }
            }
        }

        Ok(DataBlock::FixedWidth(FixedWidthDataBlock {
            data: LanceBuffer::from(dest),
            bits_per_value: self.uncompressed_bits_per_value,
            num_values: num_rows,
        }))
    }
}

fn is_encoded_item_negative(src: &Bytes, src_idx: usize, src_offset: u64, num_bits: usize) -> bool {
    let mut last_byte_idx = src_idx + ((src_offset as usize + num_bits) / 8);
    let shift_amount = (src_offset as usize + num_bits) % 8;
    let shift_amount = if shift_amount == 0 {
        last_byte_idx -= 1;
        7
    } else {
        shift_amount - 1
    };
    let last_byte = src[last_byte_idx];
    let sign_bit_mask = 1 << shift_amount;
    let sign_bit = last_byte & sign_bit_mask;

    sign_bit > 0
}

#[derive(Debug, PartialEq)]
struct BufferStartOffset {
    index: usize,
    bit_offset: u8,
}

#[derive(Debug, PartialEq)]
enum StartOffset {
    // skip the full buffer. The value is how many rows are skipped
    // by skipping the full buffer (e.g., # rows in buffer)
    SkipFull(u64),

    // skip to some start offset in the buffer
    SkipSome(BufferStartOffset),
}

/// compute how far ahead in this buffer should we skip ahead and start reading
///
/// * `rows_to_skip` - how many rows to skip
/// * `buffer_len` - length buf buffer (in bytes)
/// * `bits_per_value` - number of bits used to represent a single bitpacked value
/// * `buffer_start_bit_offset` - offset of the start of the first value within the
///     buffer's  first byte
/// * `buffer_end_bit_offset` - end bit of the last value within the buffer. Can be
///     `None` if the end of the last value is byte aligned with end of buffer.
fn compute_start_offset(
    rows_to_skip: u64,
    buffer_len: usize,
    bits_per_value: u64,
    buffer_start_bit_offset: u8,
    buffer_end_bit_offset: Option<u8>,
) -> StartOffset {
    let rows_in_buffer = rows_in_buffer(
        buffer_len,
        bits_per_value,
        buffer_start_bit_offset,
        buffer_end_bit_offset,
    );
    if rows_to_skip >= rows_in_buffer {
        return StartOffset::SkipFull(rows_in_buffer);
    }

    let start_bit = rows_to_skip * bits_per_value + buffer_start_bit_offset as u64;
    let start_byte = start_bit / 8;

    StartOffset::SkipSome(BufferStartOffset {
        index: start_byte as usize,
        bit_offset: (start_bit % 8) as u8,
    })
}

/// calculates the number of rows in a buffer
fn rows_in_buffer(
    buffer_len: usize,
    bits_per_value: u64,
    buffer_start_bit_offset: u8,
    buffer_end_bit_offset: Option<u8>,
) -> u64 {
    let mut bits_in_buffer = (buffer_len * 8) as u64 - buffer_start_bit_offset as u64;

    // if the end of the last value of the buffer isn't byte aligned, subtract the
    // end offset from the total number of bits in buffer
    if let Some(buffer_end_bit_offset) = buffer_end_bit_offset {
        bits_in_buffer -= (8 - buffer_end_bit_offset) as u64;
    }

    bits_in_buffer / bits_per_value
}

#[cfg(test)]
pub mod test {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{
        types::{UInt16Type, UInt8Type},
        Float64Array, Int32Array,
    };

    use lance_datagen::{array::fill, gen, ArrayGenerator, ArrayGeneratorExt, RowCount};

    #[test]
    fn test_bitpack_params() {
        fn gen_array(generator: Box<dyn ArrayGenerator>) -> ArrayRef {
            let arr = gen()
                .anon_col(generator)
                .into_batch_rows(RowCount::from(10000))
                .unwrap()
                .column(0)
                .clone();

            arr
        }

        macro_rules! do_test {
            ($num_bits:expr, $data_type:ident, $null_probability:expr) => {
                let max = 1 << $num_bits - 1;
                let mut arr =
                    gen_array(fill::<$data_type>(max).with_random_nulls($null_probability));

                // ensure we don't randomly generate all nulls, that won't work
                while arr.null_count() == arr.len() {
                    arr = gen_array(fill::<$data_type>(max).with_random_nulls($null_probability));
                }
                let result = bitpack_params(arr);
                assert!(result.is_some());
                assert_eq!($num_bits, result.unwrap().num_bits);
            };
        }

        let test_cases = vec![
            (5u64, 0.0f64),
            (5u64, 0.9f64),
            (1u64, 0.0f64),
            (1u64, 0.5f64),
            (8u64, 0.0f64),
            (8u64, 0.5f64),
        ];

        for (num_bits, null_probability) in &test_cases {
            do_test!(*num_bits, UInt8Type, *null_probability);
            do_test!(*num_bits, UInt16Type, *null_probability);
            do_test!(*num_bits, UInt32Type, *null_probability);
            do_test!(*num_bits, UInt64Type, *null_probability);
        }

        // do some test cases that that will only work on larger types
        let test_cases = vec![
            (13u64, 0.0f64),
            (13u64, 0.5f64),
            (16u64, 0.0f64),
            (16u64, 0.5f64),
        ];
        for (num_bits, null_probability) in &test_cases {
            do_test!(*num_bits, UInt16Type, *null_probability);
            do_test!(*num_bits, UInt32Type, *null_probability);
            do_test!(*num_bits, UInt64Type, *null_probability);
        }
        let test_cases = vec![
            (25u64, 0.0f64),
            (25u64, 0.5f64),
            (32u64, 0.0f64),
            (32u64, 0.5f64),
        ];
        for (num_bits, null_probability) in &test_cases {
            do_test!(*num_bits, UInt32Type, *null_probability);
            do_test!(*num_bits, UInt64Type, *null_probability);
        }
        let test_cases = vec![
            (48u64, 0.0f64),
            (48u64, 0.5f64),
            (64u64, 0.0f64),
            (64u64, 0.5f64),
        ];
        for (num_bits, null_probability) in &test_cases {
            do_test!(*num_bits, UInt64Type, *null_probability);
        }

        // test that it returns None for datatypes that don't support bitpacking
        let arr = Float64Array::from_iter_values(vec![0.1, 0.2, 0.3]);
        let result = bitpack_params(Arc::new(arr));
        assert!(result.is_none());
    }

    #[test]
    fn test_num_compressed_bits_signed_types() {
        let values = Int32Array::from(vec![1, 2, -7]);
        let arr = Arc::new(values);
        let result = bitpack_params(arr);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(4, result.num_bits);
        assert!(result.signed);

        // check that it doesn't add a sign bit if it doesn't need to
        let values = Int32Array::from(vec![1, 2, 7]);
        let arr = Arc::new(values);
        let result = bitpack_params(arr);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(3, result.num_bits);
        assert!(!result.signed);
    }

    #[test]
    fn test_rows_in_buffer() {
        let test_cases = vec![
            (5usize, 5u64, 0u8, None, 8u64),
            (2, 3, 0, Some(5), 4),
            (2, 3, 7, Some(6), 2),
        ];

        for (
            buffer_len,
            bits_per_value,
            buffer_start_bit_offset,
            buffer_end_bit_offset,
            expected,
        ) in test_cases
        {
            let result = rows_in_buffer(
                buffer_len,
                bits_per_value,
                buffer_start_bit_offset,
                buffer_end_bit_offset,
            );
            assert_eq!(expected, result);
        }
    }

    #[test]
    fn test_compute_start_offset() {
        let result = compute_start_offset(0, 5, 5, 0, None);
        assert_eq!(
            StartOffset::SkipSome(BufferStartOffset {
                index: 0,
                bit_offset: 0
            }),
            result
        );

        let result = compute_start_offset(10, 5, 5, 0, None);
        assert_eq!(StartOffset::SkipFull(8), result);
    }
}
