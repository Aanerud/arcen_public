use apple_cf::cf::{CFArray, CFData, CFDictionary, CFNumber, CFString};
use apple_cf::cm::format_description::{
    metadata_description_keys, metadata_format_types, metadata_specification_keys,
};
use apple_cf::cm::CMMetadataFormatDescription;
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

fn main() {
    let keys = CFArray::from_values(&[&boxed_metadata_key(1, "title")]);
    let keyed =
        CMMetadataFormatDescription::create_with_keys(metadata_format_types::BOXED, Some(&keys))
            .expect("metadata description from keys");
    assert!(keyed.key_with_local_id(1).is_some());

    let specs = CFArray::from_values(&[&boxed_metadata_spec(
        "mdta/com.example.title",
        "com.apple.metadata.datatype.UTF-8",
        "en-US",
    )]);
    let specified = CMMetadataFormatDescription::create_with_metadata_specifications(
        metadata_format_types::BOXED,
        &specs,
    )
    .expect("metadata description from specifications");
    assert_eq!(specified.identifiers().expect("identifiers").len(), 1);

    let merged = keyed
        .merge(&specified)
        .expect("merged metadata description");
    assert_eq!(merged.identifiers().expect("merged identifiers").len(), 2);
}
