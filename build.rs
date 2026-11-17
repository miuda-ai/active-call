fn main() {
    // `lele`'s aarch64-apple-darwin kernels call Accelerate's `cblas_sgemm`
    // directly (see lele::kernels::gemm) but do not emit the link directive
    // themselves, so the Accelerate framework has to be linked by the final
    // binary. Without this the `ringback-detection` feature fails to link on
    // Apple Silicon.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    if std::env::var_os("CARGO_FEATURE_RINGBACK_DETECTION").is_some() {
        println!("cargo:rustc-link-lib=framework=Accelerate");
    }
}
