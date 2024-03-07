// use std::{env, path::PathBuf};

fn main() {
    tonic_build::compile_protos("plugin.proto").unwrap();
}
