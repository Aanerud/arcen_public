#include <CommonCrypto/CommonDigest.h>
#include <CoreFoundation/CoreFoundation.h>
#include <Security/Security.h>

#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define ARCEN_MAX_PROFILE_BYTES (16U * 1024U * 1024U)

enum VerificationResult {
    ARCEN_CMS_VERIFIED = 0,
    ARCEN_CMS_INPUT_ERROR = 1,
    ARCEN_CMS_USAGE_ERROR = 2,
    ARCEN_CMS_INVALID = 3,
    ARCEN_CMS_UNTRUSTED_SIGNER = 4,
};

static void secure_zero(void *buffer, size_t length) {
    static void *(*const volatile secure_memset)(void *, int, size_t) = memset;
    if (buffer != NULL && length != 0) {
        secure_memset(buffer, 0, length);
    }
}

static const uint8_t kReviewedRootDigests[][CC_SHA256_DIGEST_LENGTH] = {
    {0xb0, 0xb1, 0x73, 0x0e, 0xcb, 0xc7, 0xff, 0x45, 0x05, 0x14, 0x2c,
     0x49, 0xf1, 0x29, 0x5e, 0x6e, 0xda, 0x6b, 0xca, 0xed, 0x7e, 0x2c,
     0x68, 0xc5, 0xbe, 0x91, 0xb5, 0xa1, 0x10, 0x01, 0xf0, 0x24},
    {0xc2, 0xb9, 0xb0, 0x42, 0xdd, 0x57, 0x83, 0x0e, 0x7d, 0x11, 0x7d,
     0xac, 0x55, 0xac, 0x8a, 0xe1, 0x94, 0x07, 0xd3, 0x8e, 0x41, 0xd8,
     0x8f, 0x32, 0x15, 0xbc, 0x3a, 0x89, 0x04, 0x44, 0xa0, 0x50},
    {0x63, 0x34, 0x3a, 0xbf, 0xb8, 0x9a, 0x6a, 0x03, 0xeb, 0xb5, 0x7e,
     0x9b, 0x3f, 0x5f, 0xa7, 0xbe, 0x7c, 0x4f, 0x5c, 0x75, 0x6f, 0x30,
     0x17, 0xb3, 0xa8, 0xc4, 0x88, 0xc3, 0x65, 0x3e, 0x91, 0x79},
};

static const uint8_t kReviewedProvisioningIntermediateDigests[][CC_SHA256_DIGEST_LENGTH] = {
    {0x9e, 0xd4, 0xb3, 0xb8, 0x8c, 0x6a, 0x33, 0x9c, 0xf1, 0x38, 0x78,
     0x95, 0xbd, 0xa9, 0xca, 0x6e, 0xa3, 0x1a, 0x6b, 0x5c, 0xe9, 0xed,
     0xf7, 0x51, 0x18, 0x45, 0x92, 0x3b, 0x0c, 0x8a, 0xc9, 0x4c},
    {0xdc, 0xf2, 0x18, 0x78, 0xc7, 0x7f, 0x41, 0x98, 0xe4, 0xb4, 0x61,
     0x4f, 0x03, 0xd6, 0x96, 0xd8, 0x9c, 0x66, 0xc6, 0x60, 0x08, 0xd4,
     0x24, 0x4e, 0x1b, 0x99, 0x16, 0x1a, 0xac, 0x91, 0x60, 0x1f},
    {0xea, 0x47, 0x57, 0x88, 0x55, 0x38, 0xdd, 0x8c, 0xb5, 0x9f, 0xf4,
     0x55, 0x6f, 0x67, 0x60, 0x87, 0xd8, 0x3c, 0x85, 0xe7, 0x09, 0x02,
     0xc1, 0x22, 0xe4, 0x2c, 0x08, 0x08, 0xb5, 0xbc, 0xe1, 0x4c},
    {0x53, 0xfd, 0x00, 0x82, 0x78, 0xe5, 0xa5, 0x95, 0xfe, 0x1e, 0x90,
     0x8a, 0xe9, 0xc5, 0xe5, 0x67, 0x5f, 0x26, 0x24, 0x32, 0x64, 0xa5,
     0xa6, 0x43, 0x8c, 0x02, 0x3e, 0x3c, 0xe2, 0x87, 0x07, 0x60},
    {0xbd, 0xd4, 0xed, 0x6e, 0x74, 0x69, 0x1f, 0x0c, 0x2b, 0xfd, 0x01,
     0xbe, 0x02, 0x96, 0x19, 0x7a, 0xf1, 0x37, 0x9e, 0x04, 0x18, 0xe2,
     0xd3, 0x00, 0xef, 0xa9, 0xc3, 0xbe, 0xf6, 0x42, 0xca, 0x30},
    {0x12, 0x8a, 0x8d, 0x3f, 0xd5, 0x8a, 0x44, 0xf5, 0x16, 0x04, 0x1b,
     0xb0, 0x0a, 0x0a, 0xb9, 0x78, 0x1b, 0xad, 0xec, 0x97, 0x4b, 0x11,
     0xc9, 0x07, 0xb2, 0x02, 0x7f, 0x2c, 0xc4, 0xcf, 0xbe, 0x1f},
};

static bool read_profile(const char *path, uint8_t **bytes, size_t *length) {
    int descriptor = open(path, O_RDONLY | O_CLOEXEC);
    if (descriptor < 0) {
        return false;
    }

    struct stat metadata;
    if (fstat(descriptor, &metadata) != 0 || metadata.st_size <= 0 ||
        (uint64_t)metadata.st_size > ARCEN_MAX_PROFILE_BYTES) {
        close(descriptor);
        return false;
    }

    size_t expected = (size_t)metadata.st_size;
    uint8_t *buffer = calloc(expected, 1);
    if (buffer == NULL) {
        close(descriptor);
        return false;
    }

    size_t offset = 0;
    while (offset < expected) {
        ssize_t received = read(descriptor, buffer + offset, expected - offset);
        if (received <= 0) {
            secure_zero(buffer, expected);
            free(buffer);
            close(descriptor);
            return false;
        }
        offset += (size_t)received;
    }
    close(descriptor);
    *bytes = buffer;
    *length = expected;
    return true;
}

static bool digest_is_reviewed(
    const uint8_t digest[CC_SHA256_DIGEST_LENGTH],
    const uint8_t reviewed[][CC_SHA256_DIGEST_LENGTH],
    size_t reviewed_count) {
    for (size_t index = 0; index < reviewed_count; ++index) {
        uint8_t difference = 0;
        for (size_t byte = 0; byte < CC_SHA256_DIGEST_LENGTH; ++byte) {
            difference |= digest[byte] ^ reviewed[index][byte];
        }
        if (difference == 0) {
            return true;
        }
    }
    return false;
}

static bool certificate_is_reviewed(
    SecCertificateRef certificate,
    const uint8_t reviewed[][CC_SHA256_DIGEST_LENGTH],
    size_t reviewed_count) {
    if (certificate == NULL) {
        return false;
    }
    CFDataRef der = SecCertificateCopyData(certificate);
    if (der == NULL || CFDataGetLength(der) > UINT32_MAX) {
        if (der != NULL) {
            CFRelease(der);
        }
        return false;
    }
    uint8_t digest[CC_SHA256_DIGEST_LENGTH] = {0};
    CC_SHA256(
        CFDataGetBytePtr(der),
        (CC_LONG)CFDataGetLength(der),
        digest);
    CFRelease(der);
    bool accepted = digest_is_reviewed(digest, reviewed, reviewed_count);
    secure_zero(digest, sizeof(digest));
    return accepted;
}

static bool is_macos_profile_signer(SecCertificateRef signer) {
    CFStringRef common_name = NULL;
    if (SecCertificateCopyCommonName(signer, &common_name) != errSecSuccess ||
        common_name == NULL) {
        return false;
    }
    bool accepted =
        CFEqual(common_name, CFSTR("Mac OS X Provisioning Profile Signing"));
    CFRelease(common_name);
    return accepted;
}

static bool has_reviewed_provisioning_chain(SecTrustRef trust) {
    CFIndex certificate_count = SecTrustGetCertificateCount(trust);
    if (certificate_count < 3) {
        return false;
    }
    SecCertificateRef signer = SecTrustGetCertificateAtIndex(trust, 0);
    SecCertificateRef root =
        SecTrustGetCertificateAtIndex(trust, certificate_count - 1);
    if (!is_macos_profile_signer(signer) ||
        !certificate_is_reviewed(
            root,
            kReviewedRootDigests,
            sizeof(kReviewedRootDigests) / sizeof(kReviewedRootDigests[0]))) {
        return false;
    }
    for (CFIndex index = 1; index < certificate_count - 1; ++index) {
        if (certificate_is_reviewed(
                SecTrustGetCertificateAtIndex(trust, index),
                kReviewedProvisioningIntermediateDigests,
                sizeof(kReviewedProvisioningIntermediateDigests) /
                    sizeof(kReviewedProvisioningIntermediateDigests[0]))) {
            return true;
        }
    }
    return false;
}

static enum VerificationResult verify_profile(const uint8_t *bytes, size_t length) {
    CMSDecoderRef decoder = NULL;
    if (CMSDecoderCreate(&decoder) != errSecSuccess || decoder == NULL) {
        return ARCEN_CMS_INVALID;
    }

    enum VerificationResult result = ARCEN_CMS_INVALID;
    if (CMSDecoderUpdateMessage(decoder, bytes, length) != errSecSuccess ||
        CMSDecoderFinalizeMessage(decoder) != errSecSuccess) {
        goto cleanup;
    }

    size_t signer_count = 0;
    if (CMSDecoderGetNumSigners(decoder, &signer_count) != errSecSuccess ||
        signer_count != 1) {
        goto cleanup;
    }

    SecPolicyRef policy = SecPolicyCreateBasicX509();
    if (policy == NULL) {
        goto cleanup;
    }
    CMSSignerStatus signer_status = kCMSSignerUnsigned;
    SecTrustRef trust = NULL;
    OSStatus status = CMSDecoderCopySignerStatus(
        decoder,
        0,
        policy,
        false,
        &signer_status,
        &trust,
        NULL);
    CFRelease(policy);
    if (status != errSecSuccess || signer_status != kCMSSignerValid ||
        trust == NULL) {
        if (trust != NULL) {
            CFRelease(trust);
        }
        goto cleanup;
    }

    result = ARCEN_CMS_UNTRUSTED_SIGNER;
    CFErrorRef trust_error = NULL;
    bool trusted = SecTrustEvaluateWithError(trust, &trust_error);
    if (trust_error != NULL) {
        CFRelease(trust_error);
    }
    if (trusted && has_reviewed_provisioning_chain(trust)) {
        result = ARCEN_CMS_VERIFIED;
    }
    CFRelease(trust);

cleanup:
    CFRelease(decoder);
    return result;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        return ARCEN_CMS_USAGE_ERROR;
    }

    uint8_t *bytes = NULL;
    size_t length = 0;
    if (!read_profile(argv[1], &bytes, &length)) {
        return ARCEN_CMS_INPUT_ERROR;
    }
    enum VerificationResult result = verify_profile(bytes, length);
    secure_zero(bytes, length);
    free(bytes);
    return result;
}
