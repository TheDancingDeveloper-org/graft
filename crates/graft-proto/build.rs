//! Generates the gRPC code from `proto/` with `protox`, so no `protoc` is
//! needed on the builder.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/graft/v1/graft.proto";
    println!("cargo:rerun-if-changed={proto}");
    let fds = protox::compile([proto], ["proto"])?;
    tonic_prost_build::configure()
        .btree_map(".")
        // Tokens never appear in `Debug` output; `pb.rs` redacts these.
        .skip_debug(["graft.v1.Token", "graft.v1.ExchangeCiTokenRequest"])
        .compile_fds(fds)?;
    Ok(())
}
