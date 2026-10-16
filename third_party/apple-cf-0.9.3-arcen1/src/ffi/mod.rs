//! Raw FFI declarations for the Swift bridge.
//!
//! These are intentionally low-level (`*mut c_void` + scalar arguments) and
//! `unsafe`; safe wrappers live in [`crate::iosurface`], [`crate::dispatch_queue`],
//! etc.
//!
//! Currently exposes:
//! * `acf_free_string` — heap-string deallocator used by every bridge fn that
//!   returns an owned C string back to Rust.
//! * `dispatch_queue_*` — Grand Central Dispatch queue lifetime + creation.
//! * `io_surface_*` — `IOSurface` accessors and lifetime control.
//!
//! Future framework additions (`CoreMedia`, `CoreVideo`, Metal, ...) extend this
//! module and gain matching `@_cdecl` exports under
//! `swift-bridge/Sources/<Framework>Bridge/`.

#![allow(missing_docs)]

mod core_foundation;
mod core_video_extras;
mod dispatch_extras;

pub use core_foundation::*;
pub use core_video_extras::*;
pub use dispatch_extras::*;

use core::ffi::{c_char, c_void};

extern "C" {
    /// Free a heap-allocated C string previously returned by any bridge fn.
    /// Centralised in `AppleCFBridge.swift`.
    pub fn acf_free_string(s: *mut c_char);

    // ---- dispatch ----
    /// Swift bridge function `acf_dispatch_queue_create` for the corresponding Apple API.
    pub fn acf_dispatch_queue_create(label: *const c_char, qos: i32) -> *const c_void;
    /// Swift bridge function `dispatch_queue_release` for the corresponding Apple API.
    pub fn dispatch_queue_release(queue: *const c_void);
    /// Swift bridge function `dispatch_queue_retain` for the corresponding Apple API.
    pub fn dispatch_queue_retain(queue: *const c_void) -> *const c_void;

    // ---- CoreGraphics bridge helpers ----
    /// Swift bridge function `cgimage_save_png` for the corresponding Apple API.
    pub fn cgimage_save_png(image: *mut c_void, path: *const c_char) -> bool;

    // ---- IOSurface ----
    /// Swift bridge function `io_surface_create` for the corresponding Apple API.
    pub fn io_surface_create(
        width: usize,
        height: usize,
        pixel_format: u32,
        bytes_per_element: usize,
        out_surface: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `io_surface_create_with_properties` for the corresponding Apple API.
    pub fn io_surface_create_with_properties(
        width: usize,
        height: usize,
        pixel_format: u32,
        bytes_per_element: usize,
        bytes_per_row: usize,
        alloc_size: usize,
        plane_count: usize,
        plane_widths: *const usize,
        plane_heights: *const usize,
        plane_bytes_per_row: *const usize,
        plane_bytes_per_element: *const usize,
        plane_offsets: *const usize,
        plane_sizes: *const usize,
        surface_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `io_surface_release` for the corresponding Apple API.
    pub fn io_surface_release(surface: *mut c_void);
    /// Swift bridge function `io_surface_retain` for the corresponding Apple API.
    pub fn io_surface_retain(surface: *mut c_void) -> *mut c_void;
    /// Swift bridge function `io_surface_hash` for the corresponding Apple API.
    pub fn io_surface_hash(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_width` for the corresponding Apple API.
    pub fn io_surface_get_width(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_height` for the corresponding Apple API.
    pub fn io_surface_get_height(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_bytes_per_row` for the corresponding Apple API.
    pub fn io_surface_get_bytes_per_row(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_alloc_size` for the corresponding Apple API.
    pub fn io_surface_get_alloc_size(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_pixel_format` for the corresponding Apple API.
    pub fn io_surface_get_pixel_format(surface: *mut c_void) -> u32;
    /// Swift bridge function `io_surface_get_id` for the corresponding Apple API.
    pub fn io_surface_get_id(surface: *mut c_void) -> u32;
    /// Swift bridge function `io_surface_get_seed` for the corresponding Apple API.
    pub fn io_surface_get_seed(surface: *mut c_void) -> u32;
    /// Swift bridge function `io_surface_get_plane_count` for the corresponding Apple API.
    pub fn io_surface_get_plane_count(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_width_of_plane` for the corresponding Apple API.
    pub fn io_surface_get_width_of_plane(surface: *mut c_void, plane_index: usize) -> usize;
    /// Swift bridge function `io_surface_get_height_of_plane` for the corresponding Apple API.
    pub fn io_surface_get_height_of_plane(surface: *mut c_void, plane_index: usize) -> usize;
    /// Swift bridge function `io_surface_get_bytes_per_row_of_plane` for the corresponding Apple API.
    pub fn io_surface_get_bytes_per_row_of_plane(surface: *mut c_void, plane_index: usize)
        -> usize;
    /// Swift bridge function `io_surface_get_base_address_of_plane` for the corresponding Apple API.
    pub fn io_surface_get_base_address_of_plane(
        surface: *mut c_void,
        plane_index: usize,
    ) -> *mut c_void;
    /// Swift bridge function `io_surface_get_base_address` for the corresponding Apple API.
    pub fn io_surface_get_base_address(surface: *mut c_void) -> *mut c_void;
    /// Swift bridge function `io_surface_get_bytes_per_element` for the corresponding Apple API.
    pub fn io_surface_get_bytes_per_element(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_element_width` for the corresponding Apple API.
    pub fn io_surface_get_element_width(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_get_element_height` for the corresponding Apple API.
    pub fn io_surface_get_element_height(surface: *mut c_void) -> usize;
    /// Swift bridge function `io_surface_is_in_use` for the corresponding Apple API.
    pub fn io_surface_is_in_use(surface: *mut c_void) -> bool;
    /// Swift bridge function `io_surface_increment_use_count` for the corresponding Apple API.
    pub fn io_surface_increment_use_count(surface: *mut c_void);
    /// Swift bridge function `io_surface_decrement_use_count` for the corresponding Apple API.
    pub fn io_surface_decrement_use_count(surface: *mut c_void);
    /// Swift bridge function `io_surface_lock` for the corresponding Apple API.
    pub fn io_surface_lock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    /// Swift bridge function `io_surface_unlock` for the corresponding Apple API.
    pub fn io_surface_unlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;

    // ---- CMSampleBuffer ----
    /// Swift bridge function `cm_sample_buffer_release` for the corresponding Apple API.
    pub fn cm_sample_buffer_release(sample_buffer: *mut c_void);
    /// Swift bridge function `cm_sample_buffer_retain` for the corresponding Apple API.
    pub fn cm_sample_buffer_retain(sample_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_sample_buffer_hash` for the corresponding Apple API.
    pub fn cm_sample_buffer_hash(sample_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cm_sample_buffer_get_data_buffer` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_data_buffer(sample_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_sample_buffer_get_format_description` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_format_description(sample_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_sample_buffer_get_image_buffer` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_image_buffer(sample_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_sample_buffer_get_presentation_timestamp` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_presentation_timestamp(
        sample_buffer: *mut c_void,
        out_value: *mut i64,
        out_timescale: *mut i32,
        out_flags: *mut u32,
        out_epoch: *mut i64,
    );
    /// Swift bridge function `cm_sample_buffer_get_decode_timestamp` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_decode_timestamp(
        sample_buffer: *mut c_void,
        out_value: *mut i64,
        out_timescale: *mut i32,
        out_flags: *mut u32,
        out_epoch: *mut i64,
    );
    /// Swift bridge function `cm_sample_buffer_get_duration` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_duration(
        sample_buffer: *mut c_void,
        out_value: *mut i64,
        out_timescale: *mut i32,
        out_flags: *mut u32,
        out_epoch: *mut i64,
    );
    /// Swift bridge function `cm_sample_buffer_get_num_samples` for the corresponding Apple API.
    pub fn cm_sample_buffer_get_num_samples(sample_buffer: *mut c_void) -> i64;
    /// Swift bridge function `cm_sample_buffer_is_valid` for the corresponding Apple API.
    pub fn cm_sample_buffer_is_valid(sample_buffer: *mut c_void) -> bool;
    /// Swift bridge function `cm_sample_buffer_data_is_ready` for the corresponding Apple API.
    pub fn cm_sample_buffer_data_is_ready(sample_buffer: *mut c_void) -> bool;

    // ---- CMBlockBuffer ----
    /// Swift bridge function `cm_block_buffer_release` for the corresponding Apple API.
    pub fn cm_block_buffer_release(block_buffer: *mut c_void);
    /// Swift bridge function `cm_block_buffer_retain` for the corresponding Apple API.
    pub fn cm_block_buffer_retain(block_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_block_buffer_hash` for the corresponding Apple API.
    pub fn cm_block_buffer_hash(block_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cm_block_buffer_get_data_length` for the corresponding Apple API.
    pub fn cm_block_buffer_get_data_length(block_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cm_block_buffer_is_empty` for the corresponding Apple API.
    pub fn cm_block_buffer_is_empty(block_buffer: *mut c_void) -> bool;
    /// Swift bridge function `cm_block_buffer_is_range_contiguous` for the corresponding Apple API.
    pub fn cm_block_buffer_is_range_contiguous(
        block_buffer: *mut c_void,
        offset: usize,
        length: usize,
    ) -> bool;
    /// Swift bridge function `cm_block_buffer_get_data_pointer` for the corresponding Apple API.
    pub fn cm_block_buffer_get_data_pointer(
        block_buffer: *mut c_void,
        offset: usize,
        out_length_at_offset: *mut usize,
        out_total_length: *mut usize,
        out_data_pointer: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_block_buffer_copy_data_bytes` for the corresponding Apple API.
    pub fn cm_block_buffer_copy_data_bytes(
        block_buffer: *mut c_void,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_block_buffer_create_with_data` for the corresponding Apple API.
    pub fn cm_block_buffer_create_with_data(
        data: *const c_void,
        data_length: usize,
        block_buffer_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_block_buffer_create_empty` for the corresponding Apple API.
    pub fn cm_block_buffer_create_empty(block_buffer_out: *mut *mut c_void) -> i32;

    // ---- CMFormatDescription ----
    /// Swift bridge function `cm_format_description_release` for the corresponding Apple API.
    pub fn cm_format_description_release(format_description: *mut c_void);
    /// Swift bridge function `cm_format_description_retain` for the corresponding Apple API.
    pub fn cm_format_description_retain(format_description: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cm_format_description_hash` for the corresponding Apple API.
    pub fn cm_format_description_hash(format_description: *mut c_void) -> usize;
    /// Swift bridge function `cm_format_description_get_media_type` for the corresponding Apple API.
    pub fn cm_format_description_get_media_type(format_description: *mut c_void) -> u32;
    /// Swift bridge function `cm_format_description_get_media_subtype` for the corresponding Apple API.
    pub fn cm_format_description_get_media_subtype(format_description: *mut c_void) -> u32;
    /// Swift bridge function `cm_format_description_get_extensions` for the corresponding Apple API.
    pub fn cm_format_description_get_extensions(format_description: *mut c_void) -> *const c_void;
    /// Swift bridge function `cm_format_description_get_audio_sample_rate` for the corresponding Apple API.
    pub fn cm_format_description_get_audio_sample_rate(format_description: *mut c_void) -> f64;
    /// Swift bridge function `cm_format_description_get_audio_channel_count` for the corresponding Apple API.
    pub fn cm_format_description_get_audio_channel_count(format_description: *mut c_void) -> u32;
    /// Swift bridge function `cm_format_description_get_audio_bits_per_channel` for the corresponding Apple API.
    pub fn cm_format_description_get_audio_bits_per_channel(format_description: *mut c_void)
        -> u32;
    /// Swift bridge function `cm_format_description_get_audio_bytes_per_frame` for the corresponding Apple API.
    pub fn cm_format_description_get_audio_bytes_per_frame(format_description: *mut c_void) -> u32;
    /// Swift bridge function `cm_format_description_get_audio_format_flags` for the corresponding Apple API.
    pub fn cm_format_description_get_audio_format_flags(format_description: *mut c_void) -> u32;
    /// Swift bridge function `cm_metadata_format_description_create_with_keys` for the corresponding Apple API.
    pub fn cm_metadata_format_description_create_with_keys(
        metadata_type: u32,
        keys: *mut c_void,
        format_description_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_metadata_format_description_create_with_metadata_specifications` for the corresponding Apple API.
    pub fn cm_metadata_format_description_create_with_metadata_specifications(
        metadata_type: u32,
        metadata_specifications: *mut c_void,
        format_description_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_metadata_format_description_create_with_description_and_metadata_specifications` for the corresponding Apple API.
    pub fn cm_metadata_format_description_create_with_description_and_metadata_specifications(
        source_description: *mut c_void,
        metadata_specifications: *mut c_void,
        format_description_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_metadata_format_description_create_by_merging_descriptions` for the corresponding Apple API.
    pub fn cm_metadata_format_description_create_by_merging_descriptions(
        source_description: *mut c_void,
        other_source_description: *mut c_void,
        format_description_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cm_metadata_format_description_get_identifiers` for the corresponding Apple API.
    pub fn cm_metadata_format_description_get_identifiers(
        format_description: *mut c_void,
    ) -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_get_key_with_local_id` for the corresponding Apple API.
    pub fn cm_metadata_format_description_get_key_with_local_id(
        format_description: *mut c_void,
        local_id: u32,
    ) -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_extension_key_metadata_key_table` for the corresponding Apple API.
    pub fn cm_metadata_format_description_extension_key_metadata_key_table() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_conforming_data_types` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_conforming_data_types() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_data_type` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_data_type() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_data_type_namespace` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_data_type_namespace() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_language_tag` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_language_tag() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_local_id` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_local_id() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_namespace` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_namespace() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_setup_data` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_setup_data() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_structural_dependency` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_structural_dependency() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_key_value` for the corresponding Apple API.
    pub fn cm_metadata_format_description_key_value() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_metadata_specification_key_data_type` for the corresponding Apple API.
    pub fn cm_metadata_format_description_metadata_specification_key_data_type() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_metadata_specification_key_extended_language_tag` for the corresponding Apple API.
    pub fn cm_metadata_format_description_metadata_specification_key_extended_language_tag(
    ) -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_metadata_specification_key_identifier` for the corresponding Apple API.
    pub fn cm_metadata_format_description_metadata_specification_key_identifier() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_metadata_specification_key_setup_data` for the corresponding Apple API.
    pub fn cm_metadata_format_description_metadata_specification_key_setup_data() -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_metadata_specification_key_structural_dependency` for the corresponding Apple API.
    pub fn cm_metadata_format_description_metadata_specification_key_structural_dependency(
    ) -> *mut c_void;
    /// Swift bridge function `cm_metadata_format_description_structural_dependency_key_dependency_is_invalid_flag` for the corresponding Apple API.
    pub fn cm_metadata_format_description_structural_dependency_key_dependency_is_invalid_flag(
    ) -> *mut c_void;

    // ---- CVPixelBuffer ----
    /// Swift bridge function `cv_pixel_buffer_release` for the corresponding Apple API.
    pub fn cv_pixel_buffer_release(pixel_buffer: *mut c_void);
    /// Swift bridge function `cv_pixel_buffer_retain` for the corresponding Apple API.
    pub fn cv_pixel_buffer_retain(pixel_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cv_pixel_buffer_hash` for the corresponding Apple API.
    pub fn cv_pixel_buffer_hash(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_type_id` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_type_id() -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_width` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_width(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_height` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_height(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_pixel_format_type` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_pixel_format_type(pixel_buffer: *mut c_void) -> u32;
    /// Swift bridge function `cv_pixel_buffer_get_bytes_per_row` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_bytes_per_row(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_data_size` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_data_size(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_is_planar` for the corresponding Apple API.
    pub fn cv_pixel_buffer_is_planar(pixel_buffer: *mut c_void) -> bool;
    /// Swift bridge function `cv_pixel_buffer_get_plane_count` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_plane_count(pixel_buffer: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_width_of_plane` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_width_of_plane(
        pixel_buffer: *mut c_void,
        plane_index: usize,
    ) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_height_of_plane` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_height_of_plane(
        pixel_buffer: *mut c_void,
        plane_index: usize,
    ) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_bytes_per_row_of_plane` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_bytes_per_row_of_plane(
        pixel_buffer: *mut c_void,
        plane_index: usize,
    ) -> usize;
    /// Swift bridge function `cv_pixel_buffer_get_base_address_of_plane` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_base_address_of_plane(
        pixel_buffer: *mut c_void,
        plane_index: usize,
    ) -> *mut c_void;
    /// Swift bridge function `cv_pixel_buffer_get_base_address` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_base_address(pixel_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cv_pixel_buffer_lock_base_address` for the corresponding Apple API.
    pub fn cv_pixel_buffer_lock_base_address(pixel_buffer: *mut c_void, flags: u32) -> i32;
    /// Swift bridge function `cv_pixel_buffer_unlock_base_address` for the corresponding Apple API.
    pub fn cv_pixel_buffer_unlock_base_address(pixel_buffer: *mut c_void, flags: u32) -> i32;
    /// Swift bridge function `cv_pixel_buffer_get_io_surface` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_io_surface(pixel_buffer: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cv_pixel_buffer_get_extended_pixels` for the corresponding Apple API.
    pub fn cv_pixel_buffer_get_extended_pixels(
        pixel_buffer: *mut c_void,
        extra_columns_on_left: *mut usize,
        extra_columns_on_right: *mut usize,
        extra_rows_on_top: *mut usize,
        extra_rows_on_bottom: *mut usize,
    );
    /// Swift bridge function `cv_pixel_buffer_fill_extended_pixels` for the corresponding Apple API.
    pub fn cv_pixel_buffer_fill_extended_pixels(pixel_buffer: *mut c_void) -> i32;
    /// Swift bridge function `cv_pixel_buffer_create` for the corresponding Apple API.
    pub fn cv_pixel_buffer_create(
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cv_pixel_buffer_create_with_bytes` for the corresponding Apple API.
    pub fn cv_pixel_buffer_create_with_bytes(
        width: usize,
        height: usize,
        pixel_format_type: u32,
        base_address: *mut c_void,
        bytes_per_row: usize,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cv_pixel_buffer_create_with_planar_bytes` for the corresponding Apple API.
    pub fn cv_pixel_buffer_create_with_planar_bytes(
        width: usize,
        height: usize,
        pixel_format_type: u32,
        num_planes: usize,
        plane_base_addresses: *const *mut c_void,
        plane_widths: *const usize,
        plane_heights: *const usize,
        plane_bytes_per_row: *const usize,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cv_pixel_buffer_create_with_io_surface` for the corresponding Apple API.
    pub fn cv_pixel_buffer_create_with_io_surface(
        io_surface: *mut c_void,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;

    // ---- CVPixelBufferPool ----
    /// Swift bridge function `cv_pixel_buffer_pool_release` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_release(pool: *mut c_void);
    /// Swift bridge function `cv_pixel_buffer_pool_retain` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_retain(pool: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cv_pixel_buffer_pool_hash` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_hash(pool: *mut c_void) -> usize;
    /// Swift bridge function `cv_pixel_buffer_pool_get_type_id` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_get_type_id() -> usize;
    /// Swift bridge function `cv_pixel_buffer_pool_create` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_create(
        width: usize,
        height: usize,
        pixel_format_type: u32,
        max_buffers: usize,
        pool_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cv_pixel_buffer_pool_create_pixel_buffer` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_create_pixel_buffer(
        pool: *mut c_void,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;
    /// Swift bridge function `cv_pixel_buffer_pool_flush` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_flush(pool: *mut c_void);
    /// Swift bridge function `cv_pixel_buffer_pool_get_attributes` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_get_attributes(pool: *mut c_void) -> *const c_void;
    /// Swift bridge function `cv_pixel_buffer_pool_get_pixel_buffer_attributes` for the corresponding Apple API.
    pub fn cv_pixel_buffer_pool_get_pixel_buffer_attributes(pool: *mut c_void) -> *const c_void;
}
