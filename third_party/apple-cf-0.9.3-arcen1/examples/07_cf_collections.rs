use apple_cf::cf::{
    CFArray, CFAttributedString, CFBag, CFDictionary, CFMutableSet, CFPropertyList,
    CFPropertyListFormat, CFPropertyListMutabilityOptions, CFSet, CFSetCallbacks, CFString, CFTree,
};

fn main() {
    let first = CFString::new("first");
    let second = CFString::new("second");

    let array = CFArray::from_values(&[&first, &second]);
    assert_eq!(array.len(), 2);

    let dict = CFDictionary::from_pairs(&[(&first, &second)]);
    assert!(dict.contains_key(&first));

    let bag = CFBag::from_values(&[&first, &first, &second]);
    assert_eq!(bag.count_of_value(&first), 2);

    let set = CFSet::from_values(&[&first, &second]);
    assert!(set.contains(&first));

    let mutable_set = CFMutableSet::with_callbacks(0, CFSetCallbacks::Type);
    mutable_set.add(&first);
    mutable_set.set(&second);
    assert_eq!(mutable_set.len(), 2);

    let plist = CFDictionary::from_pairs(&[(&first, &second)]);
    let data = CFPropertyList::create_data(&plist, CFPropertyListFormat::BinaryV1_0, 0)
        .expect("serialize property list");
    let (decoded, format) =
        CFPropertyList::create_with_data(&data, CFPropertyListMutabilityOptions::IMMUTABLE)
            .expect("decode property list");
    assert_eq!(format, CFPropertyListFormat::BinaryV1_0);
    assert_eq!(decoded.type_id(), CFDictionary::type_id());

    let attributed = CFAttributedString::new(&first);
    assert_eq!(attributed.string().to_string(), "first");

    let root = CFTree::new(Some(&first));
    let child = CFTree::new(Some(&second));
    root.append_child(&child);
    assert_eq!(root.child_count(), 1);
}
