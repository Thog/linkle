#[macro_use]
extern crate clap;
extern crate sprinkle;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate cargo_metadata;
extern crate cargo_toml2;
extern crate goblin;
extern crate scroll;

use scroll::IOwrite;
use std::env::{self, VarError};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cargo_metadata::{Message, Package};
use cargo_toml2::CargoConfig;
use clap::{App, Arg};
use derive_more::Display;
use failure::Fail;
use goblin::elf::section_header::{SHT_NOBITS, SHT_STRTAB, SHT_SYMTAB};
use goblin::elf::{Elf, Header as ElfHeader, ProgramHeader};
use sprinkle::format::{nacp::NacpFile, nxo::NxoFile, romfs::RomFs, pfs0::Pfs0, npdm::NpdmJson, npdm::ACIDBehavior};

#[derive(Debug, Fail, Display)]
enum Error {
    #[display(fmt = "{}", _0)]
    Goblin(#[cause] goblin::error::Error),
    #[display(fmt = "{}", _0)]
    Sprinkle(#[cause] sprinkle::error::Error),
}

impl From<goblin::error::Error> for Error {
    fn from(from: goblin::error::Error) -> Error {
        Error::Goblin(from)
    }
}

impl From<sprinkle::error::Error> for Error {
    fn from(from: sprinkle::error::Error) -> Error {
        Error::Sprinkle(from)
    }
}

impl From<std::io::Error> for Error {
    fn from(from: std::io::Error) -> Error {
        sprinkle::error::Error::from(from).into()
    }
}

// TODO: Run cargo build --help to get the list of options!
const CARGO_OPTIONS: &str = "CARGO OPTIONS:
    -p, --package <SPEC>...         Package to build
        --all                       Build all packages in the workspace
        --exclude <SPEC>...         Exclude packages from the build
    -j, --jobs <N>                  Number of parallel jobs, defaults to # of CPUs
        --lib                       Build only this package's library
        --bin <NAME>...             Build only the specified binary
        --bins                      Build all binaries
        --example <NAME>...         Build only the specified example
        --examples                  Build all examples
        --test <NAME>...            Build only the specified test target
        --tests                     Build all tests
        --bench <NAME>...           Build only the specified bench target
        --benches                   Build all benches
        --all-targets               Build all targets (lib and bin targets by default)
        --release                   Build artifacts in release mode, with optimizations
        --features <FEATURES>       Space-separated list of features to activate
        --all-features              Activate all available features
        --no-default-features       Do not activate the `default` feature
        --target <TRIPLE>           Build for the target triple
        --target-dir <DIRECTORY>    Directory for all generated artifacts
        --out-dir <PATH>            Copy final artifacts to this directory
        --manifest-path <PATH>      Path to Cargo.toml
        --message-format <FMT>      Error format [default: human]  [possible values: human, json]
        --build-plan                Output the build plan in JSON
    -v, --verbose                   Use verbose output (-vv very verbose/build.rs output)
    -q, --quiet                     No output printed to stdout
        --color <WHEN>              Coloring: auto, always, never
        --frozen                    Require Cargo.lock and cache are up to date
        --locked                    Require Cargo.lock is up to date
    -Z <FLAG>...                    Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for details
    -h, --help                      Prints help information";

trait BetterIOWrite<Ctx: Copy>: IOwrite<Ctx> {
    fn iowrite_with_try<
        N: scroll::ctx::SizeWith<Ctx, Units = usize> + scroll::ctx::TryIntoCtx<Ctx>,
    >(
        &mut self,
        n: N,
        ctx: Ctx,
    ) -> Result<(), N::Error>
    where
        N::Error: From<std::io::Error>,
    {
        let mut buf = [0u8; 256];
        let size = N::size_with(&ctx);
        let buf = &mut buf[0..size];
        n.try_into_ctx(buf, ctx)?;
        self.write_all(buf)?;
        Ok(())
    }
}

impl<Ctx: Copy, W: IOwrite<Ctx> + ?Sized> BetterIOWrite<Ctx> for W {}

fn generate_debuginfo_romfs<P: AsRef<Path>>(
    elf_path: &Path,
    romfs: Option<P>,
) -> Result<RomFs, Error> {
    let mut elf_file = File::open(elf_path)?;
    let mut buffer = Vec::new();
    elf_file.read_to_end(&mut buffer)?;
    let elf = goblin::elf::Elf::parse(&buffer)?;
    let new_file = {
        let mut new_path = PathBuf::from(elf_path);
        new_path.set_extension("debug");
        let mut file = File::create(&new_path)?;
        let Elf {
            mut header,
            program_headers,
            mut section_headers,
            is_64,
            little_endian,
            ..
        } = elf;

        let ctx = goblin::container::Ctx {
            container: if is_64 {
                goblin::container::Container::Big
            } else {
                goblin::container::Container::Little
            },
            le: if little_endian {
                goblin::container::Endian::Little
            } else {
                goblin::container::Endian::Big
            },
        };

        for section in section_headers.iter_mut() {
            if section.sh_type == SHT_NOBITS
                || section.sh_type == SHT_SYMTAB
                || section.sh_type == SHT_STRTAB
            {
                continue;
            }
            if let Some(Ok(s)) = elf.shdr_strtab.get(section.sh_name) {
                if !(s.starts_with(".debug") || s == ".comment") {
                    section.sh_type = SHT_NOBITS;
                }
            }
        }

        // Calculate section data length + elf/program headers
        let data_off = ElfHeader::size(&ctx) + ProgramHeader::size(&ctx) * program_headers.len();
        let shoff = data_off as u64
            + section_headers
                .iter()
                .map(|v| {
                    if v.sh_type != SHT_NOBITS {
                        v.sh_size
                    } else {
                        0
                    }
                })
                .sum::<u64>();

        // Write ELF header
        // TODO: Anything else?
        header.e_phoff = ::std::mem::size_of::<ElfHeader>() as u64;
        header.e_shoff = shoff;
        file.iowrite_with(header, ctx)?;

        // Write program headers
        for phdr in program_headers {
            file.iowrite_with_try(phdr, ctx)?;
        }

        // Write section data
        let mut cur_idx = data_off;
        for section in section_headers
            .iter_mut()
            .filter(|v| v.sh_type != SHT_NOBITS)
        {
            file.write_all(
                &buffer[section.sh_offset as usize..(section.sh_offset + section.sh_size) as usize],
            )?;
            section.sh_offset = cur_idx as u64;
            cur_idx += section.sh_size as usize;
        }

        // Write section headers
        for section in section_headers {
            file.iowrite_with(section, ctx)?;
        }

        file.sync_all()?;
        new_path
    };

    let mut romfs = if let Some(romfs) = romfs {
        RomFs::from_directory(romfs.as_ref())?
    } else {
        RomFs::empty()
    };

    romfs.push_file(&new_file, "debug_info.elf")?;

    Ok(romfs)
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct PackageMetadata {
    target: String,
    npdm: String
}

trait WorkspaceMember {
    fn part(&self, n: usize) -> &str;

    fn name(&self) -> &str {
        self.part(0)
    }

    fn version(&self) -> semver::Version {
        semver::Version::parse(self.part(1)).expect("bad version in cargo metadata")
    }

    fn url(&self) -> &str {
        let url = self.part(2);
        &url[1..url.len() - 1]
    }
}

impl WorkspaceMember for cargo_metadata::PackageId {
    fn part(&self, n: usize) -> &str {
        self.repr.splitn(3, ' ').nth(n).unwrap()
    }
}

fn main() {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();

    let rust_target_path = match env::var("RUST_TARGET_PATH") {
        Err(VarError::NotPresent) => metadata.workspace_root.clone(),
        s => PathBuf::from(s.unwrap()),
    };

    let mut command = Command::new("xargo");

    let config_path = Path::new("./.cargo/config");
    let target = if config_path.exists() {
        let config: Option<CargoConfig> = cargo_toml2::from_path(config_path).ok();
        config
            .map(|config| config.build.map(|build| build.target).flatten())
            .flatten()
    } else {
        None
    };

    let target = "aarch64-none-elf";

    let mut xargo_args: Vec<String> = vec![
        String::from("build"),
        format!("--target={}", target)
    ];

    for arg in env::args().skip(1) {
        xargo_args.push(arg);
    }

    command
        .args(&xargo_args)
        .stdout(Stdio::piped())
        .env("RUST_TARGET_PATH", rust_target_path.as_os_str());

    let command = command.spawn().unwrap();

    let iter = cargo_metadata::parse_messages(command.stdout.unwrap());
    for message in iter {
        match message {
            Ok(Message::CompilerArtifact(ref artifact))
                if artifact.target.kind.contains(&"bin".into())
                    || artifact.target.kind.contains(&"cdylib".into()) =>
            {
                let package: &Package = match metadata
                    .packages
                    .iter()
                    .find(|v| v.id == artifact.package_id)
                {
                    Some(v) => v,
                    None => continue,
                };

                let root = package.manifest_path.parent().unwrap();
                let target_metadata: PackageMetadata = serde_json::from_value(
                    package
                        .metadata
                        .pointer(&format!("linkle/{}", artifact.target.name))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                )
                .unwrap_or_default();

                let target_name = artifact.filenames[0].as_path().file_name().unwrap();
                let target_path = artifact.filenames[0].as_path().parent().unwrap();

                let exefs_dir = target_path.join("exefs");
                let _ = std::fs::remove_dir_all(exefs_dir.clone());
                std::fs::create_dir(exefs_dir.clone()).unwrap();

                let main_npdm = exefs_dir.join("main.npdm");
                let main_exe = exefs_dir.join("main");

                let mut exefs_nsp = artifact.filenames[0].clone();
                assert!(exefs_nsp.set_extension("nsp"));

                let npdm = NpdmJson::from_file(Path::new(&target_metadata.npdm)).unwrap();
                let mut option = OpenOptions::new();
                let output_option = option.write(true).create(true).truncate(true);
                let mut out_file = output_option.open(main_npdm.clone()).map_err(|err| (err, main_npdm.clone())).unwrap();
                npdm.into_npdm(&mut out_file, ACIDBehavior::Empty).unwrap();

                NxoFile::from_elf(artifact.filenames[0].to_str().unwrap()).unwrap().write_nso(&mut File::create(main_exe.clone()).unwrap()).unwrap();

                let mut nsp = Pfs0::from_directory(exefs_dir.to_str().unwrap()).unwrap();
                let mut option = OpenOptions::new();
                let output_option = option.write(true).create(true).truncate(true);
                nsp.write_pfs0(
                    &mut output_option
                        .open(exefs_nsp.clone())
                        .map_err(|err| (err, exefs_nsp.clone())).unwrap(),
                )
                .map_err(|err| (err, exefs_nsp.clone())).unwrap();

                println!("Built {}", exefs_nsp.to_string_lossy());
            }
            Ok(Message::CompilerArtifact(_artifact)) => {
                //println!("{:#?}", artifact);
            }
            Ok(Message::CompilerMessage(msg)) => {
                if let Some(msg) = msg.message.rendered {
                    println!("{}", msg);
                } else {
                    println!("{:?}", msg);
                }
            }
            Ok(_) => (),
            Err(err) => {
                panic!("{:?}", err);
            }
        }
    }
}
