//! One cfg alias for the dma-buf fast path: `dmabuf` means Linux *and* the
//! `dma-buf` feature, so sources read `#[cfg(dmabuf)]` instead of repeating the
//! three-part condition.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(dmabuf)");
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    let feature = std::env::var_os("CARGO_FEATURE_DMA_BUF").is_some();
    if linux && feature {
        println!("cargo:rustc-cfg=dmabuf");
    }
}
