use apple_cf::cf::{CFArray, CFData, CFDictionary, CFNumber, CFString};
use apple_cf::cm::format_description::{
    format_description_extension_keys, metadata_description_keys, metadata_format_types,
    metadata_specification_keys, metadata_structural_dependency_keys,
};
use apple_cf::cm::{CMFormatDescription, CMMetadataFormatDescription};
use apple_cf::FourCharCode;

fn boxed_metadata_key(local_id: u64, value: &str) -> CFDictionary {
    let namespace_key = metadata_description_keys::namespace();
    let value_key = metadata_description_keys::value();
    let local_id_key = metadata_description_keys::local_id();
    let namespace = CFNumber::from_u64(u64::from(FourCharCode::from_bytes(*b"mdta").as_u32()));
    let value = CFData::from_bytes(value.as_bytes());
    let local_id = CFNumber::from_u64(local_id);
    CFDictionary::from_pairs(&[
        (&namespace_key, &namespace),
        (&value_key, &value),
        (&local_id_key, &local_id),
    ])
}

fn boxed_metadata_spec(identifier: &str, data_type: &str, language_tag: &str) -> CFDictionary {
    let identifier_key = metadata_specification_keys::identifier();
    let data_type_key = metadata_specification_keys::data_type();
    let language_key = metadata_specification_keys::extended_language_tag();
    let identifier = CFString::new(identifier);
    let data_type = CFString::new(data_type);
    let language = CFString::new(language_tag);
    CFDictionary::from_pairs(&[
        (&identifier_key, &identifier),
        (&data_type_key, &data_type),
        (&language_key, &language),
    ])
}

#[test]
fn cm_metadata_format_description_helpers_work() {
    let keys = CFArray::from_values(&[&boxed_metadata_key(1, "title")]);
    let by_keys =
        CMMetadataFormatDescription::create_with_keys(metadata_format_types::BOXED, Some(&keys))
            .expect("metadata description from keys");
    assert!(by_keys.is_metadata());
    assert_eq!(by_keys.media_subtype(), metadata_format_types::BOXED);
    let key = by_keys.key_with_local_id(1).expect("key for local id");
    let local_id_key = metadata_description_keys::local_id();
    assert!(key.contains_key(&local_id_key));

    let specs = CFArray::from_values(&[&boxed_metadata_spec(
        "mdta/com.example.title",
        "com.apple.metadata.datatype.UTF-8",
        "en-US",
    )]);
    let by_specs = CMMetadataFormatDescription::create_with_metadata_specifications(
        metadata_format_types::BOXED,
        &specs,
    )
    .expect("metadata description from specifications");
    let identifiers = by_specs.identifiers().expect("metadata identifiers");
    assert_eq!(identifiers.len(), 1);
    let identifier = identifiers.get(0).expect("identifier");
    let identifier = unsafe { CFString::from_raw_retained(identifier.as_ptr()) }
        .expect("metadata identifier string");
    assert_eq!(identifier.to_string(), "mdta/com.example.title");

    let extended = by_keys
        .extend_with_metadata_specifications(&specs)
        .expect("extended metadata description");
    assert_eq!(
        extended.identifiers().expect("extended identifiers").len(),
        2
    );

    let merged = by_keys
        .merge(&by_specs)
        .expect("merged metadata description");
    assert_eq!(merged.identifiers().expect("merged identifiers").len(), 2);

    let format_description: CMFormatDescription = merged.into();
    let metadata = CMMetadataFormatDescription::try_from(format_description)
        .expect("metadata-specific format description");
    assert!(metadata.is_metadata());

    assert!(!format_description_extension_keys::metadata_key_table().is_empty());
    assert!(!metadata_structural_dependency_keys::dependency_is_invalid_flag().is_empty());
}
