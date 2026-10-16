use apple_cf::raw;

#[test]
fn raw_bindings_cover_remaining_inline_and_metal_dispatch_symbols() {
    let range = raw::CFRangeMake(7, 11);
    assert_eq!(range.location, 7);
    assert_eq!(range.length, 11);

    let order = raw::CFByteOrderGetCurrent();
    assert!(matches!(
        order,
        raw::CFByteOrderUnknown | raw::CFByteOrderLittleEndian | raw::CFByteOrderBigEndian
    ));
    assert_eq!(raw::CFSwapInt16(0x1234), 0x3412);
    assert_eq!(raw::CFSwapInt32(0x1234_5678), 0x7856_3412);

    let mut surrogates = [0_u16; 2];
    assert_eq!(
        unsafe { raw::CFStringGetSurrogatePairForLongCharacter(0x1F600, surrogates.as_mut_ptr()) },
        1
    );
    assert_eq!(
        raw::CFStringGetLongCharacterForSurrogatePair(surrogates[0], surrogates[1]),
        0x1F600
    );
    assert_eq!(raw::CFStringIsSurrogateHighCharacter(surrogates[0]), 1);
    assert_eq!(raw::CFStringIsSurrogateLowCharacter(surrogates[1]), 1);

    assert_eq!(raw::CFUserNotificationCheckBoxChecked(0), 1 << 8);
    assert_eq!(raw::CFUserNotificationSecureTextField(0), 1 << 16);
    assert_eq!(raw::CFUserNotificationPopUpSelection(3), 3 << 24);

    let tag = unsafe { raw::kCMTagMediaTypeVideo };
    assert_eq!(raw::CMTagIsValid(tag), 1);
    assert_eq!(
        raw::CMTagGetCategory(tag),
        raw::kCMTagCategory_MediaType as raw::CMTagCategory
    );
    assert_eq!(
        raw::CMTagHasCategory(tag, raw::kCMTagCategory_MediaType as raw::CMTagCategory),
        1
    );
    assert_eq!(raw::CMTagCategoryEqualToTagCategory(tag, tag), 1);
    assert_eq!(raw::CMTagCategoryValueEqualToValue(tag, tag), 1);

    let main_queue = unsafe { raw::dispatch_get_main_queue() };
    assert!(!main_queue.is_null());

    let _ = raw::CMTimebaseCreateWithMasterTimebase;
    let _ = raw::CMTimebaseSetMasterClock;
    let _ = raw::CMTimebaseSetMasterTimebase;
    let _ = raw::CVMetalBufferGetBuffer;
    let _ = raw::CVMetalBufferCacheCreate;
    let _ = raw::CVMetalTextureGetTypeID;
    let _ = raw::CVMetalTextureGetTexture;
    let _ = raw::CVMetalTextureIsFlipped;
    let _ = raw::CVMetalTextureGetCleanTexCoords;
    let _ = raw::CVMetalTextureCacheCreateTextureFromImage;
}
