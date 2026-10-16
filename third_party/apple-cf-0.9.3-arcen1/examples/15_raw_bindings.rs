use apple_cf::raw;

fn main() {
    let range = raw::CFRangeMake(0, 4);
    assert_eq!(range.length, 4);

    let order = raw::CFByteOrderGetCurrent();
    assert!(matches!(
        order,
        raw::CFByteOrderUnknown | raw::CFByteOrderLittleEndian | raw::CFByteOrderBigEndian
    ));

    let mut surrogates = [0_u16; 2];
    assert_eq!(
        unsafe { raw::CFStringGetSurrogatePairForLongCharacter(0x1F600, surrogates.as_mut_ptr()) },
        1
    );
    assert_eq!(
        raw::CFStringGetLongCharacterForSurrogatePair(surrogates[0], surrogates[1]),
        0x1F600
    );

    let tag = unsafe { raw::kCMTagMediaTypeVideo };
    assert_eq!(raw::CMTagIsValid(tag), 1);

    let main_queue = unsafe { raw::dispatch_get_main_queue() };
    assert!(!main_queue.is_null());

    let _ = raw::CVMetalTextureGetTypeID;
    let _ = raw::CVMetalTextureGetTexture;
    let _ = raw::CVMetalTextureIsFlipped;
    let _ = raw::CVMetalTextureGetCleanTexCoords;
    let _ = raw::CVMetalTextureCacheCreateTextureFromImage;
    let _ = raw::CVMetalBufferGetBuffer;
    let _ = raw::CVMetalBufferCacheCreate;
}
