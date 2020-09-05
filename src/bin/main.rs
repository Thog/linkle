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
use std::path::PathBuf;
use std::process::{Command, Stdio};

use cargo_metadata::{Message, Package};
use derive_more::Display;
use failure::Fail;
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

#[derive(Debug, Serialize, Deserialize, Default)]
struct NspMetadata {
    npdm: String
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct NroMetadata {
    romfs: Option<String>,
    icon: Option<String>,
    nacp: Option<NacpFile>
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

enum Format {
    NSP,
    NRO
}

fn main() {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();

    let fmt = match env::args().nth(1) {
        Some(fmt) => match fmt.as_str() {
            "nsp" => {
                println!("Building NSP sysmodule...");
                Format::NSP
            },
            "nro" => {
                println!("Building NRO binary...");
                Format::NRO
            }
            _ => panic!("Unknown format type (available types: nsp, nro)"),
        },
        None => panic!("No format argument was specified"),
    };

    let rust_target_path = match env::var("RUST_TARGET_PATH") {
        Err(VarError::NotPresent) => metadata.workspace_root.clone(),
        s => PathBuf::from(s.unwrap()),
    };

    let target = "aarch64-none-elf";
    let mut command = Command::new("xargo");

    let mut xargo_args: Vec<String> = vec![
        String::from("build"),
        format!("--target={}", target),
        String::from("--message-format=json-diagnostic-rendered-ansi"),
    ];

    // Forward other arguments to xargo
    for arg in env::args().skip(2) {
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

                match fmt {
                    Format::NSP => {
                        let target_metadata: NspMetadata = serde_json::from_value(
                            package
                                .metadata
                                .pointer("/sprinkle/nsp")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                        .unwrap_or_default();
        
                        let target_path = artifact.filenames[0].parent().unwrap();
        
                        let exefs_dir = target_path.join("exefs");
                        let _ = std::fs::remove_dir_all(exefs_dir.clone());
                        std::fs::create_dir(exefs_dir.clone()).unwrap();
        
                        let main_npdm = exefs_dir.join("main.npdm");
                        let main_exe = exefs_dir.join("main");
        
                        let mut exefs_nsp = artifact.filenames[0].clone();
                        assert!(exefs_nsp.set_extension("nsp"));
        
                        let npdm_json = root.join(target_metadata.npdm.clone());
                        let npdm = NpdmJson::from_file(&npdm_json).unwrap();
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
                    },
                    Format::NRO => {
                        let target_metadata: NroMetadata = serde_json::from_value(
                            package
                                .metadata
                                .pointer("/sprinkle/nro")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                        .unwrap_or_default();
        
                        let mut nro = artifact.filenames[0].clone();
                        assert!(nro.set_extension("nro"));

                        let romfs = target_metadata.romfs.as_ref().map(|romfs_dir| RomFs::from_directory(&root.join(romfs_dir)).unwrap());
                        let icon = target_metadata.icon.map(|icon_file| root.join(icon_file.clone())).map(|icon_path| icon_path.to_string_lossy().into_owned());

                        NxoFile::from_elf(artifact.filenames[0].to_str().unwrap())
                        .unwrap()
                        .write_nro(
                            &mut File::create(nro.clone()).unwrap(),
                            romfs,
                            icon.as_ref().map(|icon_path| icon_path.as_str()),
                            target_metadata.nacp,
                        )
                        .unwrap();
                        
        
                        println!("Built {}", nro.to_string_lossy());
                    }
                };
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
