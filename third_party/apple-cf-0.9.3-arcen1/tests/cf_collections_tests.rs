use apple_cf::cf::{
    CFArray, CFAttributedString, CFBag, CFDictionary, CFMutableSet, CFPropertyList,
    CFPropertyListFormat, CFPropertyListMutabilityOptions, CFSet, CFSetCallbacks, CFStreamPair,
    CFString, CFTree,
};

#[test]
fn cf_collection_wrappers_work() {
    let first = CFString::new("first");
    let second = CFString::new("second");

    let array = CFArray::from_values(&[&first, &second]);
    assert_eq!(array.len(), 2);
    assert_eq!(array.values().len(), 2);

    let dictionary = CFDictionary::from_pairs(&[(&first, &second)]);
    assert!(dictionary.contains_key(&first));
    assert_eq!(dictionary.keys().len(), 1);
    assert_eq!(dictionary.values().len(), 1);

    let bag = CFBag::from_values(&[&first, &first, &second]);
    assert_eq!(bag.count_of_value(&first), 2);
    assert!(bag.contains(&second));

    let set = CFSet::from_values(&[&first, &second]);
    assert_eq!(set.len(), 2);
    assert!(set.contains(&first));
    let mut seen = Vec::new();
    set.for_each(|value| seen.push(value.description()));
    seen.sort();
    assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);

    let mutable_set = CFMutableSet::with_callbacks(0, CFSetCallbacks::Type);
    mutable_set.add(&first);
    mutable_set.set(&second);
    assert_eq!(mutable_set.len(), 2);
    mutable_set.remove(&first);
    assert!(!mutable_set.contains(&first));
    mutable_set.clear();
    assert!(mutable_set.is_empty());

    let plist = CFDictionary::from_pairs(&[(&first, &second)]);
    assert!(CFPropertyList::is_valid(
        &plist,
        CFPropertyListFormat::BinaryV1_0
    ));
    let deep_copy =
        CFPropertyList::create_deep_copy(&plist, CFPropertyListMutabilityOptions::IMMUTABLE)
            .expect("deep copy plist");
    assert_eq!(deep_copy.type_id(), CFDictionary::type_id());

    let data = CFPropertyList::create_data(&plist, CFPropertyListFormat::BinaryV1_0, 0)
        .expect("serialize plist");
    let (decoded, detected_format) =
        CFPropertyList::create_with_data(&data, CFPropertyListMutabilityOptions::IMMUTABLE)
            .expect("decode plist");
    assert_eq!(detected_format, CFPropertyListFormat::BinaryV1_0);
    assert_eq!(decoded.type_id(), CFDictionary::type_id());

    let streams = CFStreamPair::new(1024);
    assert!(streams.read.open());
    assert!(streams.write.open());
    let written = CFPropertyList::write(&plist, &streams.write, CFPropertyListFormat::XmlV1_0, 0)
        .expect("write plist to stream");
    streams.write.close();
    let (stream_decoded, stream_format) = CFPropertyList::create_with_stream(
        &streams.read,
        written,
        CFPropertyListMutabilityOptions::IMMUTABLE,
    )
    .expect("decode plist from stream");
    streams.read.close();
    assert_eq!(stream_format, CFPropertyListFormat::XmlV1_0);
    assert_eq!(stream_decoded.type_id(), CFDictionary::type_id());

    let attributed = CFAttributedString::new(&first);
    assert_eq!(attributed.string().to_string(), "first");

    let root = CFTree::new(Some(&first));
    let child = CFTree::new(Some(&second));
    root.append_child(&child);
    assert_eq!(root.child_count(), 1);
    assert!(root.child_at(0).is_some());
    assert!(root.value().is_some());
}
