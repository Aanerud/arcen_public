use apple_cf::cv::CVMetalTextureCache;

fn main() {
    if let Some(cache) = CVMetalTextureCache::system_default() {
        cache.flush();
    }
}
