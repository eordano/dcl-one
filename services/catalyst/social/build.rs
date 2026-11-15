use std::fmt::Write as _;
use std::io::Result;
use std::path::{Path, PathBuf};

/// The repository root, from the [env] table of the root .cargo/config.toml.
fn root() -> PathBuf {
    PathBuf::from(std::env::var("DCL_ONE_ROOT").expect("DCL_ONE_ROOT"))
}

/// The social RPC messages, compiled from the one proto source
/// (`library/protocol/proto`) without protoc: protox parses, prost-build emits
/// one module per package into OUT_DIR (`decentraland.common.rs`,
/// `decentraland.social_service.rs`, `decentraland.social_service.v2.rs`),
/// which src/rpc/proto/mod.rs includes.
fn main() -> Result<()> {
    println!("cargo:rerun-if-env-changed=DCL_ONE_ROOT");
    let include = root().join("library/protocol/proto");
    println!("cargo:rerun-if-changed={}", include.display());
    let files: Vec<PathBuf> = [
        "decentraland/common/colors.proto",
        "decentraland/social_service/errors.proto",
        "decentraland/social_service/v2/social_service_v2.proto",
    ]
    .iter()
    .map(|f| include.join(f))
    .collect();
    let fds = compile_protos(&files, &include)?;
    prost_build::Config::new()
        .service_generator(Box::new(RegistrationContract))
        .compile_fds(fds)
}

fn protox_error(e: protox::Error) -> std::io::Error {
    std::io::Error::other(format!("{e:?}"))
}

/// What `protoc --include_imports -o` produces, without protoc; the google
/// resolver serves `google/protobuf/empty.proto`.
fn compile_protos(files: &[PathBuf], include: &Path) -> Result<prost_types::FileDescriptorSet> {
    let mut resolver = protox::file::ChainFileResolver::new();
    resolver.add(protox::file::IncludeFileResolver::new(
        include.to_path_buf(),
    ));
    resolver.add(protox::file::GoogleFileResolver::new());
    let mut compiler = protox::Compiler::with_file_resolver(resolver);
    compiler.include_imports(true);
    compiler.open_files(files).map_err(protox_error)?;
    Ok(compiler.file_descriptor_set())
}

/// Compile test-only assertions from the descriptor into the generated v2 module.
/// The manual registration macros invoke them with their actual decoded request
/// and returned response, so type or streaming changes cannot pass the name test.
struct RegistrationContract;

impl prost_build::ServiceGenerator for RegistrationContract {
    fn generate(&mut self, service: prost_build::Service, output: &mut String) {
        assert_eq!(service.package, "decentraland.social_service.v2");
        assert_eq!(service.proto_name, "SocialService");
        let mut arms = String::new();
        let mut names = Vec::new();
        for method in service.methods {
            names.push(method.proto_name.clone());
            let kind = match (method.client_streaming, method.server_streaming) {
                (false, false) => "unary",
                (false, true) => "server_stream",
                (true, false) => "client_stream",
                (true, true) => "bidirectional_stream",
            };
            for (direction, ty) in [
                ("request", method.input_type),
                ("response", method.output_type),
            ] {
                let function = format!("assert_{}_{}", method.name, direction);
                writeln!(
                    output,
                    "#[cfg(test)] pub(crate) fn {function}(_: &{ty}) {{}}"
                )
                .unwrap();
                writeln!(arms,
                    "({name:?}, {kind}, {direction}, $value:expr) => {{ $crate::rpc::proto::v2::{function}($value) }};",
                    name = method.proto_name,
                ).unwrap();
            }
        }
        writeln!(output,
            "#[cfg(test)] macro_rules! assert_rpc_contract {{ {arms} }}\n#[cfg(test)] pub(crate) use assert_rpc_contract;"
        ).unwrap();
        writeln!(
            output,
            "#[cfg(test)] pub(crate) const RPC_PROCEDURES: &[&str] = &{names:?};"
        )
        .unwrap();
    }
}
