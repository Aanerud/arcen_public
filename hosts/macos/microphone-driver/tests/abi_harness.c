#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreAudio/AudioHardware.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef void *(*FactoryFn)(CFAllocatorRef, CFUUIDRef);

static int fail(const char *message) {
    fprintf(stderr, "abi harness: %s\n", message);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2) return fail("usage: abi_harness <driver dylib>");
    if (sizeof(AudioBufferList) != 24) return fail("unexpected AudioBufferList size");
    if (offsetof(AudioBufferList, mBuffers) != 8) return fail("unexpected AudioBufferList buffers offset");

    void *handle = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!handle) return fail(dlerror());
    FactoryFn factory = (FactoryFn)dlsym(handle, "ArcenMicrophoneFactory");
    if (!factory) return fail("missing factory symbol");

    CFUUIDRef plugin_type = kAudioServerPlugInTypeUUID;
    AudioServerPlugInDriverRef driver = (AudioServerPlugInDriverRef)factory(kCFAllocatorDefault, plugin_type);
    if (!driver || !*driver) return fail("factory rejected plug-in type UUID");

    LPVOID iface = NULL;
    CFUUIDBytes interface_uuid = CFUUIDGetUUIDBytes(kAudioServerPlugInDriverInterfaceUUID);
    HRESULT hr = (*driver)->QueryInterface(driver, interface_uuid, &iface);
    if (hr != 0 || iface == NULL) return fail("QueryInterface rejected driver interface UUID by value");

    AudioObjectPropertyAddress address = {
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeInput,
        kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    OSStatus status = (*driver)->GetPropertyDataSize(driver, 2, 0, &address, 0, NULL, &size);
    if (status != 0 || size != sizeof(AudioBufferList)) return fail("stream config size mismatch");

    struct {
        uint8_t bytes[sizeof(AudioBufferList)];
        uint32_t canary;
    } storage;
    memset(&storage, 0xa5, sizeof(storage));
    storage.canary = 0xfeedface;
    UInt32 out_size = 0;
    status = (*driver)->GetPropertyData(driver, 2, 0, &address, 0, NULL, size, &out_size, storage.bytes);
    if (status != 0 || out_size != sizeof(AudioBufferList)) return fail("stream config get failed");
    if (storage.canary != 0xfeedface) return fail("stream config wrote past buffer");

    AudioBufferList *list = (AudioBufferList *)storage.bytes;
    if (list->mNumberBuffers != 1 || list->mBuffers[0].mNumberChannels != 1) return fail("stream config contents wrong");

    address.mSelector = kAudioPlugInPropertyDeviceList;
    status = (*driver)->GetPropertyDataSize(driver, 1, 0, &address, 0, NULL, &size);
    if (status != 0 || size != sizeof(AudioObjectID)) return fail("device list size mismatch");

    (*driver)->Release(driver);
    dlclose(handle);
    return 0;
}
