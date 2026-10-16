#![allow(missing_docs)]

use core::ffi::{c_char, c_void};

extern "C" {
    /// Swift bridge function `acf_object_release` for the corresponding Apple API.
    pub fn acf_object_release(value: *mut c_void);
    /// Swift bridge function `acf_object_retain` for the corresponding Apple API.
    pub fn acf_object_retain(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `acf_object_hash` for the corresponding Apple API.
    pub fn acf_object_hash(value: *mut c_void) -> usize;

    /// Swift bridge function `cf_type_release` for the corresponding Apple API.
    pub fn cf_type_release(value: *mut c_void);
    /// Swift bridge function `cf_type_retain` for the corresponding Apple API.
    pub fn cf_type_retain(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_type_hash` for the corresponding Apple API.
    pub fn cf_type_hash(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_type_equal` for the corresponding Apple API.
    pub fn cf_type_equal(lhs: *mut c_void, rhs: *mut c_void) -> bool;
    /// Swift bridge function `cf_type_get_type_id` for the corresponding Apple API.
    pub fn cf_type_get_type_id(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_type_copy_description` for the corresponding Apple API.
    pub fn cf_type_copy_description(value: *mut c_void) -> *mut c_char;

    /// Swift bridge function `cf_string_get_type_id` for the corresponding Apple API.
    pub fn cf_string_get_type_id() -> usize;
    /// Swift bridge function `cf_string_create_with_cstring` for the corresponding Apple API.
    pub fn cf_string_create_with_cstring(value: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_string_copy_cstring` for the corresponding Apple API.
    pub fn cf_string_copy_cstring(value: *mut c_void) -> *mut c_char;
    /// Swift bridge function `cf_string_get_length` for the corresponding Apple API.
    pub fn cf_string_get_length(value: *mut c_void) -> usize;

    /// Swift bridge function `cf_number_get_type_id` for the corresponding Apple API.
    pub fn cf_number_get_type_id() -> usize;
    /// Swift bridge function `cf_number_create_i64` for the corresponding Apple API.
    pub fn cf_number_create_i64(value: i64) -> *mut c_void;
    /// Swift bridge function `cf_number_create_u64` for the corresponding Apple API.
    pub fn cf_number_create_u64(value: u64) -> *mut c_void;
    /// Swift bridge function `cf_number_create_f64` for the corresponding Apple API.
    pub fn cf_number_create_f64(value: f64) -> *mut c_void;
    /// Swift bridge function `cf_number_get_i64` for the corresponding Apple API.
    pub fn cf_number_get_i64(value: *mut c_void, out: *mut i64) -> bool;
    /// Swift bridge function `cf_number_get_u64` for the corresponding Apple API.
    pub fn cf_number_get_u64(value: *mut c_void, out: *mut u64) -> bool;
    /// Swift bridge function `cf_number_get_f64` for the corresponding Apple API.
    pub fn cf_number_get_f64(value: *mut c_void, out: *mut f64) -> bool;
    /// Swift bridge function `cf_number_is_float_type` for the corresponding Apple API.
    pub fn cf_number_is_float_type(value: *mut c_void) -> bool;

    /// Swift bridge function `cf_data_get_type_id` for the corresponding Apple API.
    pub fn cf_data_get_type_id() -> usize;
    /// Swift bridge function `cf_data_create` for the corresponding Apple API.
    pub fn cf_data_create(bytes: *const u8, len: usize) -> *mut c_void;
    /// Swift bridge function `cf_data_get_length` for the corresponding Apple API.
    pub fn cf_data_get_length(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_data_copy_bytes` for the corresponding Apple API.
    pub fn cf_data_copy_bytes(value: *mut c_void, buffer: *mut u8);

    /// Swift bridge function `cf_date_get_type_id` for the corresponding Apple API.
    pub fn cf_date_get_type_id() -> usize;
    /// Swift bridge function `cf_date_create` for the corresponding Apple API.
    pub fn cf_date_create(absolute_time: f64) -> *mut c_void;
    /// Swift bridge function `cf_date_get_absolute_time` for the corresponding Apple API.
    pub fn cf_date_get_absolute_time(value: *mut c_void) -> f64;

    /// Swift bridge function `cf_uuid_get_type_id` for the corresponding Apple API.
    pub fn cf_uuid_get_type_id() -> usize;
    /// Swift bridge function `cf_uuid_create` for the corresponding Apple API.
    pub fn cf_uuid_create() -> *mut c_void;
    /// Swift bridge function `cf_uuid_create_from_string` for the corresponding Apple API.
    pub fn cf_uuid_create_from_string(value: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_uuid_copy_string` for the corresponding Apple API.
    pub fn cf_uuid_copy_string(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_uuid_get_bytes` for the corresponding Apple API.
    pub fn cf_uuid_get_bytes(value: *mut c_void, out_bytes: *mut u8);

    /// Swift bridge function `cf_error_get_type_id` for the corresponding Apple API.
    pub fn cf_error_get_type_id() -> usize;
    /// Swift bridge function `cf_error_create` for the corresponding Apple API.
    pub fn cf_error_create(
        domain: *mut c_void,
        code: i64,
        description: *const c_char,
    ) -> *mut c_void;
    /// Swift bridge function `cf_error_get_domain` for the corresponding Apple API.
    pub fn cf_error_get_domain(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_error_get_code` for the corresponding Apple API.
    pub fn cf_error_get_code(value: *mut c_void) -> i64;
    /// Swift bridge function `cf_error_copy_description` for the corresponding Apple API.
    pub fn cf_error_copy_description(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_error_copy_failure_reason` for the corresponding Apple API.
    pub fn cf_error_copy_failure_reason(value: *mut c_void) -> *mut c_void;

    /// Swift bridge function `cf_url_get_type_id` for the corresponding Apple API.
    pub fn cf_url_get_type_id() -> usize;
    /// Swift bridge function `cf_url_create_with_string` for the corresponding Apple API.
    pub fn cf_url_create_with_string(value: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_url_create_file_path` for the corresponding Apple API.
    pub fn cf_url_create_file_path(path: *const c_char, is_directory: bool) -> *mut c_void;
    /// Swift bridge function `cf_url_copy_absolute_string` for the corresponding Apple API.
    pub fn cf_url_copy_absolute_string(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_url_copy_file_system_path` for the corresponding Apple API.
    pub fn cf_url_copy_file_system_path(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_url_has_directory_path` for the corresponding Apple API.
    pub fn cf_url_has_directory_path(value: *mut c_void) -> bool;

    /// Swift bridge function `cf_bundle_get_type_id` for the corresponding Apple API.
    pub fn cf_bundle_get_type_id() -> usize;
    /// Swift bridge function `cf_bundle_get_main` for the corresponding Apple API.
    pub fn cf_bundle_get_main() -> *mut c_void;
    /// Swift bridge function `cf_bundle_create` for the corresponding Apple API.
    pub fn cf_bundle_create(url: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_bundle_copy_identifier` for the corresponding Apple API.
    pub fn cf_bundle_copy_identifier(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_bundle_copy_bundle_url` for the corresponding Apple API.
    pub fn cf_bundle_copy_bundle_url(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_bundle_copy_resource_url` for the corresponding Apple API.
    pub fn cf_bundle_copy_resource_url(
        value: *mut c_void,
        name: *const c_char,
        extension: *const c_char,
        subdir: *const c_char,
    ) -> *mut c_void;

    /// Swift bridge function `cf_locale_get_type_id` for the corresponding Apple API.
    pub fn cf_locale_get_type_id() -> usize;
    /// Swift bridge function `cf_locale_copy_current` for the corresponding Apple API.
    pub fn cf_locale_copy_current() -> *mut c_void;
    /// Swift bridge function `cf_locale_create` for the corresponding Apple API.
    pub fn cf_locale_create(identifier: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_locale_copy_identifier` for the corresponding Apple API.
    pub fn cf_locale_copy_identifier(value: *mut c_void) -> *mut c_void;

    /// Swift bridge function `cf_calendar_get_type_id` for the corresponding Apple API.
    pub fn cf_calendar_get_type_id() -> usize;
    /// Swift bridge function `cf_calendar_copy_current` for the corresponding Apple API.
    pub fn cf_calendar_copy_current() -> *mut c_void;
    /// Swift bridge function `cf_calendar_create` for the corresponding Apple API.
    pub fn cf_calendar_create(identifier: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_calendar_copy_identifier` for the corresponding Apple API.
    pub fn cf_calendar_copy_identifier(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_calendar_copy_time_zone` for the corresponding Apple API.
    pub fn cf_calendar_copy_time_zone(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_calendar_set_time_zone` for the corresponding Apple API.
    pub fn cf_calendar_set_time_zone(value: *mut c_void, time_zone: *mut c_void);

    /// Swift bridge function `cf_time_zone_get_type_id` for the corresponding Apple API.
    pub fn cf_time_zone_get_type_id() -> usize;
    /// Swift bridge function `cf_time_zone_copy_current` for the corresponding Apple API.
    pub fn cf_time_zone_copy_current() -> *mut c_void;
    /// Swift bridge function `cf_time_zone_create` for the corresponding Apple API.
    pub fn cf_time_zone_create(name: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_time_zone_copy_name` for the corresponding Apple API.
    pub fn cf_time_zone_copy_name(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_time_zone_get_seconds_from_gmt` for the corresponding Apple API.
    pub fn cf_time_zone_get_seconds_from_gmt(value: *mut c_void, date: *mut c_void) -> i32;

    /// Swift bridge function `cf_character_set_get_type_id` for the corresponding Apple API.
    pub fn cf_character_set_get_type_id() -> usize;
    /// Swift bridge function `cf_character_set_create_with_characters_in_string` for the corresponding Apple API.
    pub fn cf_character_set_create_with_characters_in_string(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_character_set_create_inverted_set` for the corresponding Apple API.
    pub fn cf_character_set_create_inverted_set(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_character_set_is_character_member` for the corresponding Apple API.
    pub fn cf_character_set_is_character_member(value: *mut c_void, scalar: u32) -> bool;

    /// Swift bridge function `cf_number_formatter_get_type_id` for the corresponding Apple API.
    pub fn cf_number_formatter_get_type_id() -> usize;
    /// Swift bridge function `cf_number_formatter_create` for the corresponding Apple API.
    pub fn cf_number_formatter_create(locale: *mut c_void, style: i32) -> *mut c_void;
    /// Swift bridge function `cf_number_formatter_create_string_with_number` for the corresponding Apple API.
    pub fn cf_number_formatter_create_string_with_number(
        value: *mut c_void,
        number: *mut c_void,
    ) -> *mut c_void;
    /// Swift bridge function `cf_number_formatter_create_number_from_string` for the corresponding Apple API.
    pub fn cf_number_formatter_create_number_from_string(
        value: *mut c_void,
        string: *mut c_void,
    ) -> *mut c_void;

    /// Swift bridge function `cf_date_formatter_get_type_id` for the corresponding Apple API.
    pub fn cf_date_formatter_get_type_id() -> usize;
    /// Swift bridge function `cf_date_formatter_create` for the corresponding Apple API.
    pub fn cf_date_formatter_create(
        locale: *mut c_void,
        date_style: i32,
        time_style: i32,
    ) -> *mut c_void;
    /// Swift bridge function `cf_date_formatter_create_string_with_date` for the corresponding Apple API.
    pub fn cf_date_formatter_create_string_with_date(
        value: *mut c_void,
        date: *mut c_void,
    ) -> *mut c_void;

    /// Swift bridge function `cf_file_security_get_type_id` for the corresponding Apple API.
    pub fn cf_file_security_get_type_id() -> usize;
    /// Swift bridge function `cf_file_security_create` for the corresponding Apple API.
    pub fn cf_file_security_create() -> *mut c_void;
    /// Swift bridge function `cf_file_security_copy_owner_uuid` for the corresponding Apple API.
    pub fn cf_file_security_copy_owner_uuid(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_file_security_set_owner_uuid` for the corresponding Apple API.
    pub fn cf_file_security_set_owner_uuid(value: *mut c_void, uuid: *mut c_void) -> bool;
    /// Swift bridge function `cf_file_security_get_mode` for the corresponding Apple API.
    pub fn cf_file_security_get_mode(value: *mut c_void, out_mode: *mut u32) -> bool;
    /// Swift bridge function `cf_file_security_set_mode` for the corresponding Apple API.
    pub fn cf_file_security_set_mode(value: *mut c_void, mode: u32) -> bool;

    /// Swift bridge function `cf_property_list_create_deep_copy` for the corresponding Apple API.
    pub fn cf_property_list_create_deep_copy(value: *mut c_void, options: u64) -> *mut c_void;
    /// Swift bridge function `cf_property_list_create_with_data` for the corresponding Apple API.
    pub fn cf_property_list_create_with_data(
        data: *mut c_void,
        options: u64,
        out_format: *mut isize,
        out_error: *mut *mut c_void,
    ) -> *mut c_void;
    /// Swift bridge function `cf_property_list_create_with_stream` for the corresponding Apple API.
    pub fn cf_property_list_create_with_stream(
        stream: *mut c_void,
        stream_length: isize,
        options: u64,
        out_format: *mut isize,
        out_error: *mut *mut c_void,
    ) -> *mut c_void;
    /// Swift bridge function `cf_property_list_write` for the corresponding Apple API.
    pub fn cf_property_list_write(
        value: *mut c_void,
        stream: *mut c_void,
        format: isize,
        options: u64,
        out_error: *mut *mut c_void,
    ) -> isize;
    /// Swift bridge function `cf_property_list_create_data` for the corresponding Apple API.
    pub fn cf_property_list_create_data(
        value: *mut c_void,
        format: isize,
        options: u64,
        out_error: *mut *mut c_void,
    ) -> *mut c_void;
    /// Swift bridge function `cf_property_list_is_valid` for the corresponding Apple API.
    pub fn cf_property_list_is_valid(value: *mut c_void, format: isize) -> bool;

    /// Swift bridge function `cf_array_get_type_id` for the corresponding Apple API.
    pub fn cf_array_get_type_id() -> usize;
    /// Swift bridge function `cf_array_create` for the corresponding Apple API.
    pub fn cf_array_create(values: *const *mut c_void, count: usize) -> *mut c_void;
    /// Swift bridge function `cf_array_get_count` for the corresponding Apple API.
    pub fn cf_array_get_count(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_array_get_value_at_index` for the corresponding Apple API.
    pub fn cf_array_get_value_at_index(value: *mut c_void, index: usize) -> *mut c_void;

    /// Swift bridge function `cf_dictionary_get_type_id` for the corresponding Apple API.
    pub fn cf_dictionary_get_type_id() -> usize;
    /// Swift bridge function `cf_dictionary_create` for the corresponding Apple API.
    pub fn cf_dictionary_create(
        keys: *const *mut c_void,
        values: *const *mut c_void,
        count: usize,
    ) -> *mut c_void;
    /// Swift bridge function `cf_dictionary_get_count` for the corresponding Apple API.
    pub fn cf_dictionary_get_count(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_dictionary_contains_key` for the corresponding Apple API.
    pub fn cf_dictionary_contains_key(value: *mut c_void, key: *mut c_void) -> bool;
    /// Swift bridge function `cf_dictionary_get_value` for the corresponding Apple API.
    pub fn cf_dictionary_get_value(value: *mut c_void, key: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_dictionary_copy_keys` for the corresponding Apple API.
    pub fn cf_dictionary_copy_keys(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_dictionary_copy_values` for the corresponding Apple API.
    pub fn cf_dictionary_copy_values(value: *mut c_void) -> *mut c_void;

    /// Swift bridge function `cf_bag_get_type_id` for the corresponding Apple API.
    pub fn cf_bag_get_type_id() -> usize;
    /// Swift bridge function `cf_bag_create` for the corresponding Apple API.
    pub fn cf_bag_create(values: *const *mut c_void, count: usize) -> *mut c_void;
    /// Swift bridge function `cf_bag_get_count` for the corresponding Apple API.
    pub fn cf_bag_get_count(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_bag_contains_value` for the corresponding Apple API.
    pub fn cf_bag_contains_value(value: *mut c_void, candidate: *mut c_void) -> bool;
    /// Swift bridge function `cf_bag_get_count_of_value` for the corresponding Apple API.
    pub fn cf_bag_get_count_of_value(value: *mut c_void, candidate: *mut c_void) -> usize;

    /// Swift bridge function `cf_set_get_type_id` for the corresponding Apple API.
    pub fn cf_set_get_type_id() -> usize;
    /// Swift bridge function `cf_set_create` for the corresponding Apple API.
    pub fn cf_set_create(values: *const *mut c_void, count: usize, callbacks: i32) -> *mut c_void;
    /// Swift bridge function `cf_set_create_copy` for the corresponding Apple API.
    pub fn cf_set_create_copy(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_set_create_mutable` for the corresponding Apple API.
    pub fn cf_set_create_mutable(capacity: usize, callbacks: i32) -> *mut c_void;
    /// Swift bridge function `cf_set_create_mutable_copy` for the corresponding Apple API.
    pub fn cf_set_create_mutable_copy(value: *mut c_void, capacity: usize) -> *mut c_void;
    /// Swift bridge function `cf_set_get_count` for the corresponding Apple API.
    pub fn cf_set_get_count(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_set_get_count_of_value` for the corresponding Apple API.
    pub fn cf_set_get_count_of_value(value: *mut c_void, candidate: *mut c_void) -> usize;
    /// Swift bridge function `cf_set_contains_value` for the corresponding Apple API.
    pub fn cf_set_contains_value(value: *mut c_void, candidate: *mut c_void) -> bool;
    /// Swift bridge function `cf_set_get_value` for the corresponding Apple API.
    pub fn cf_set_get_value(value: *mut c_void, candidate: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_set_get_value_if_present` for the corresponding Apple API.
    pub fn cf_set_get_value_if_present(
        value: *mut c_void,
        candidate: *mut c_void,
        out_value: *mut *mut c_void,
    ) -> bool;
    /// Swift bridge function `cf_set_get_values` for the corresponding Apple API.
    pub fn cf_set_get_values(value: *mut c_void, out_values: *mut *mut c_void);
    /// Swift bridge function `cf_set_apply_function` for the corresponding Apple API.
    pub fn cf_set_apply_function(
        value: *mut c_void,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *mut c_void),
    );
    /// Swift bridge function `cf_set_add_value` for the corresponding Apple API.
    pub fn cf_set_add_value(value: *mut c_void, candidate: *mut c_void);
    /// Swift bridge function `cf_set_replace_value` for the corresponding Apple API.
    pub fn cf_set_replace_value(value: *mut c_void, candidate: *mut c_void);
    /// Swift bridge function `cf_set_set_value` for the corresponding Apple API.
    pub fn cf_set_set_value(value: *mut c_void, candidate: *mut c_void);
    /// Swift bridge function `cf_set_remove_value` for the corresponding Apple API.
    pub fn cf_set_remove_value(value: *mut c_void, candidate: *mut c_void);
    /// Swift bridge function `cf_set_remove_all_values` for the corresponding Apple API.
    pub fn cf_set_remove_all_values(value: *mut c_void);

    /// Swift bridge function `cf_attributed_string_get_type_id` for the corresponding Apple API.
    pub fn cf_attributed_string_get_type_id() -> usize;
    /// Swift bridge function `cf_attributed_string_create` for the corresponding Apple API.
    pub fn cf_attributed_string_create(string: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_attributed_string_get_string` for the corresponding Apple API.
    pub fn cf_attributed_string_get_string(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_attributed_string_get_length` for the corresponding Apple API.
    pub fn cf_attributed_string_get_length(value: *mut c_void) -> usize;

    /// Swift bridge function `cf_tree_create` for the corresponding Apple API.
    pub fn cf_tree_create(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_tree_append_child` for the corresponding Apple API.
    pub fn cf_tree_append_child(parent: *mut c_void, child: *mut c_void);
    /// Swift bridge function `cf_tree_get_child_count` for the corresponding Apple API.
    pub fn cf_tree_get_child_count(value: *mut c_void) -> usize;
    /// Swift bridge function `cf_tree_get_child_at_index` for the corresponding Apple API.
    pub fn cf_tree_get_child_at_index(value: *mut c_void, index: usize) -> *mut c_void;
    /// Swift bridge function `cf_tree_copy_value` for the corresponding Apple API.
    pub fn cf_tree_copy_value(value: *mut c_void) -> *mut c_void;

    /// Swift bridge function `cf_preferences_set_app_value` for the corresponding Apple API.
    pub fn cf_preferences_set_app_value(key: *mut c_void, value: *mut c_void, app_id: *mut c_void);
    /// Swift bridge function `cf_preferences_copy_app_value` for the corresponding Apple API.
    pub fn cf_preferences_copy_app_value(key: *mut c_void, app_id: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_preferences_app_synchronize` for the corresponding Apple API.
    pub fn cf_preferences_app_synchronize(app_id: *mut c_void) -> bool;

    /// Swift bridge function `cf_notification_center_get_type_id` for the corresponding Apple API.
    pub fn cf_notification_center_get_type_id() -> usize;
    /// Swift bridge function `cf_notification_center_get_local` for the corresponding Apple API.
    pub fn cf_notification_center_get_local() -> *mut c_void;
    /// Swift bridge function `cf_notification_center_get_distributed` for the corresponding Apple API.
    pub fn cf_notification_center_get_distributed() -> *mut c_void;
    /// Swift bridge function `cf_notification_center_get_darwin` for the corresponding Apple API.
    pub fn cf_notification_center_get_darwin() -> *mut c_void;
    /// Swift bridge function `cf_notification_center_post_notification` for the corresponding Apple API.
    pub fn cf_notification_center_post_notification(
        center: *mut c_void,
        name: *mut c_void,
        user_info: *mut c_void,
        deliver_immediately: bool,
    );

    /// Swift bridge function `cf_run_loop_get_type_id` for the corresponding Apple API.
    pub fn cf_run_loop_get_type_id() -> usize;
    /// Swift bridge function `cf_run_loop_get_current` for the corresponding Apple API.
    pub fn cf_run_loop_get_current() -> *mut c_void;
    /// Swift bridge function `cf_run_loop_get_main` for the corresponding Apple API.
    pub fn cf_run_loop_get_main() -> *mut c_void;
    /// Swift bridge function `cf_run_loop_run_in_default_mode` for the corresponding Apple API.
    pub fn cf_run_loop_run_in_default_mode(seconds: f64, return_after_source_handled: bool) -> i32;
    /// Swift bridge function `cf_run_loop_stop` for the corresponding Apple API.
    pub fn cf_run_loop_stop(value: *mut c_void);
    /// Swift bridge function `cf_run_loop_wake_up` for the corresponding Apple API.
    pub fn cf_run_loop_wake_up(value: *mut c_void);
    /// Swift bridge function `cf_run_loop_add_timer` for the corresponding Apple API.
    pub fn cf_run_loop_add_timer(value: *mut c_void, timer: *mut c_void);

    /// Swift bridge function `cf_run_loop_timer_get_type_id` for the corresponding Apple API.
    pub fn cf_run_loop_timer_get_type_id() -> usize;
    /// Swift bridge function `cf_run_loop_timer_create` for the corresponding Apple API.
    pub fn cf_run_loop_timer_create(interval_seconds: f64, repeats: bool) -> *mut c_void;
    /// Swift bridge function `cf_run_loop_timer_is_valid` for the corresponding Apple API.
    pub fn cf_run_loop_timer_is_valid(value: *mut c_void) -> bool;
    /// Swift bridge function `cf_run_loop_timer_invalidate` for the corresponding Apple API.
    pub fn cf_run_loop_timer_invalidate(value: *mut c_void);
    /// Swift bridge function `cf_run_loop_timer_fire` for the corresponding Apple API.
    pub fn cf_run_loop_timer_fire(value: *mut c_void);

    /// Swift bridge function `cf_message_port_get_type_id` for the corresponding Apple API.
    pub fn cf_message_port_get_type_id() -> usize;
    /// Swift bridge function `cf_message_port_create_echo_local` for the corresponding Apple API.
    pub fn cf_message_port_create_echo_local(name: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_message_port_create_remote` for the corresponding Apple API.
    pub fn cf_message_port_create_remote(name: *const c_char) -> *mut c_void;
    /// Swift bridge function `cf_message_port_send_request` for the corresponding Apple API.
    pub fn cf_message_port_send_request(
        value: *mut c_void,
        bytes: *const u8,
        len: usize,
        timeout_seconds: f64,
        out_bytes: *mut *mut u8,
        out_len: *mut usize,
    ) -> i32;
    /// Swift bridge function `cf_message_port_free_bytes` for the corresponding Apple API.
    pub fn cf_message_port_free_bytes(bytes: *mut u8, len: usize);
    /// Swift bridge function `cf_message_port_invalidate` for the corresponding Apple API.
    pub fn cf_message_port_invalidate(value: *mut c_void);

    /// Swift bridge function `cf_read_stream_get_type_id` for the corresponding Apple API.
    pub fn cf_read_stream_get_type_id() -> usize;
    /// Swift bridge function `cf_write_stream_get_type_id` for the corresponding Apple API.
    pub fn cf_write_stream_get_type_id() -> usize;
    /// Swift bridge function `cf_stream_create_bound_pair` for the corresponding Apple API.
    pub fn cf_stream_create_bound_pair(
        transfer_buffer_size: usize,
        out_read: *mut *mut c_void,
        out_write: *mut *mut c_void,
    );
    /// Swift bridge function `cf_read_stream_open` for the corresponding Apple API.
    pub fn cf_read_stream_open(value: *mut c_void) -> bool;
    /// Swift bridge function `cf_read_stream_close` for the corresponding Apple API.
    pub fn cf_read_stream_close(value: *mut c_void);
    /// Swift bridge function `cf_read_stream_read` for the corresponding Apple API.
    pub fn cf_read_stream_read(value: *mut c_void, buffer: *mut u8, len: usize) -> isize;
    /// Swift bridge function `cf_write_stream_open` for the corresponding Apple API.
    pub fn cf_write_stream_open(value: *mut c_void) -> bool;
    /// Swift bridge function `cf_write_stream_close` for the corresponding Apple API.
    pub fn cf_write_stream_close(value: *mut c_void);
    /// Swift bridge function `cf_write_stream_write` for the corresponding Apple API.
    pub fn cf_write_stream_write(value: *mut c_void, buffer: *const u8, len: usize) -> isize;

    /// Swift bridge function `cf_socket_get_type_id` for the corresponding Apple API.
    pub fn cf_socket_get_type_id() -> usize;
    /// Swift bridge function `cf_socket_create_udp_ipv4` for the corresponding Apple API.
    pub fn cf_socket_create_udp_ipv4() -> *mut c_void;
    /// Swift bridge function `cf_socket_get_native` for the corresponding Apple API.
    pub fn cf_socket_get_native(value: *mut c_void) -> i32;
    /// Swift bridge function `cf_socket_invalidate` for the corresponding Apple API.
    pub fn cf_socket_invalidate(value: *mut c_void);
    /// Swift bridge function `cf_socket_is_valid` for the corresponding Apple API.
    pub fn cf_socket_is_valid(value: *mut c_void) -> bool;

    /// Swift bridge function `cf_file_descriptor_get_type_id` for the corresponding Apple API.
    pub fn cf_file_descriptor_get_type_id() -> usize;
    /// Swift bridge function `cf_file_descriptor_create` for the corresponding Apple API.
    pub fn cf_file_descriptor_create(native_fd: i32, close_on_invalidate: bool) -> *mut c_void;
    /// Swift bridge function `cf_file_descriptor_get_native` for the corresponding Apple API.
    pub fn cf_file_descriptor_get_native(value: *mut c_void) -> i32;
    /// Swift bridge function `cf_file_descriptor_invalidate` for the corresponding Apple API.
    pub fn cf_file_descriptor_invalidate(value: *mut c_void);

    /// Swift bridge function `cf_xml_create_string_by_escaping_entities` for the corresponding Apple API.
    pub fn cf_xml_create_string_by_escaping_entities(value: *mut c_void) -> *mut c_void;
    /// Swift bridge function `cf_xml_create_string_by_unescaping_entities` for the corresponding Apple API.
    pub fn cf_xml_create_string_by_unescaping_entities(value: *mut c_void) -> *mut c_void;
}
