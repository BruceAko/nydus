// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

#![deny(warnings)]
#[macro_use(crate_authors)]
extern crate clap;
#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate lazy_static;
use crate::deduplicate::{
    check_bootstrap_versions_consistency, update_ctx_from_parent_bootstrap, Deduplicate,
    SqliteDatabase,
};
use std::convert::TryFrom;
use std::fs::{self, metadata, DirEntry, OpenOptions};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::result::Result::Ok;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgMatches, Command as App};
use nix::unistd::{getegid, geteuid};
use nydus::{get_build_time_info, setup_logging};
use nydus_api::{BuildTimeInfo, ConfigV2, LocalFsConfig};
use nydus_builder::{
    attributes::Attributes, parse_chunk_dict_arg, update_ctx_from_bootstrap, ArtifactStorage,
    BlobCacheGenerator, BlobCompactor, BlobManager, BootstrapManager, BuildContext, BuildOutput,
    Builder, ChunkdictBlobInfo, ChunkdictChunkInfo, ConversionType, DirectoryBuilder, Feature,
    Features, Generator, HashChunkDict, Merger, OptimizePrefetch, Prefetch, PrefetchPolicy,
    StargzBuilder, TarballBuilder, Tree, TreeNode, WhiteoutSpec,
};

use nydus_rafs::metadata::{MergeError, RafsSuper, RafsSuperConfig, RafsVersion};
use nydus_storage::backend::localfs::LocalFs;
use nydus_storage::backend::BlobBackend;
use nydus_storage::device::BlobFeatures;
use nydus_storage::factory::BlobFactory;
use nydus_storage::meta::{format_blob_features, BatchContextGenerator};
use nydus_storage::{RAFS_DEFAULT_CHUNK_SIZE, RAFS_MAX_CHUNK_SIZE};
use nydus_utils::trace::{EventTracerClass, TimingTracerClass, TraceClass};
use nydus_utils::{
    compress, digest, event_tracer, lazy_drop, register_tracer, root_tracer, timing_tracer,
};
use serde::{Deserialize, Serialize};

use crate::unpack::{OCIUnpacker, Unpacker};
use crate::validator::Validator;
use nydus_rafs::metadata::layout::v5::{RafsV5BlobTable, RafsV5ExtBlobTable};
use nydus_rafs::metadata::layout::v6::RafsV6BlobTable;

use nydus_rafs::metadata::layout::RafsBlobTable;

#[cfg(target_os = "linux")]
use nydus_service::ServiceArgs;
#[cfg(target_os = "linux")]
use std::str::FromStr;

mod deduplicate;
mod inspect;
mod stat;
mod unpack;
mod validator;

const BLOB_ID_MAXIMUM_LENGTH: usize = 255;

#[derive(Serialize, Deserialize, Default)]
pub struct OutputSerializer {
    /// The binary version of builder (nydus-image).
    version: String,
    /// RAFS meta data file path.
    bootstrap: String,
    /// Represents all blob in blob table ordered by blob index, this field
    /// only include the layer that does have a blob, and should be deprecated
    /// in future, use `artifacts` field to replace.
    blobs: Vec<String>,
    /// external RAFS meta data file path.
    external_bootstrap: String,
    /// external RAFS blob meta data file path.
    external_blobs: Vec<String>,
    /// Performance trace info for current build.
    trace: serde_json::Map<String, serde_json::Value>,
    /// RAFS filesystem version (5 or 6).
    fs_version: String,
    /// Chunk compression algorithm.
    compressor: String,
}

impl OutputSerializer {
    fn dump(
        matches: &ArgMatches,
        build_output: BuildOutput,
        build_info: &BuildTimeInfo,
        compressor: compress::Algorithm,
        fs_version: RafsVersion,
    ) -> Result<()> {
        let output_json: Option<PathBuf> = matches
            .get_one::<String>("output-json")
            .map(|o| o.to_string().into());

        if let Some(ref f) = output_json {
            let w = OpenOptions::new()
                .truncate(true)
                .create(true)
                .write(true)
                .open(f)
                .with_context(|| format!("can not open output file {}", f.display()))?;
            let trace = root_tracer!().dump_summary_map().unwrap_or_default();
            let version = format!("{}-{}", build_info.package_ver, build_info.git_commit);
            let output = Self {
                version,
                bootstrap: build_output.bootstrap_path.unwrap_or_default(),
                blobs: build_output.blobs,
                external_bootstrap: build_output.external_bootstrap_path.unwrap_or_default(),
                external_blobs: build_output.external_blobs,
                trace,
                fs_version: fs_version.to_string(),
                compressor: compressor.to_string(),
            };

            serde_json::to_writer_pretty(w, &output)
                .context("failed to write result to output file")?;
        }

        Ok(())
    }

    fn dump_for_check(
        matches: &ArgMatches,
        build_info: &BuildTimeInfo,
        blob_ids: Vec<String>,
        bootstrap: &Path,
        compressor: compress::Algorithm,
        fs_version: RafsVersion,
    ) -> Result<()> {
        let output_json: Option<PathBuf> = matches
            .get_one::<String>("output-json")
            .map(|o| o.to_string().into());

        if let Some(ref f) = output_json {
            let w = OpenOptions::new()
                .truncate(true)
                .create(true)
                .write(true)
                .open(f)
                .with_context(|| format!("can not open output file {}", f.display()))?;
            let trace = root_tracer!().dump_summary_map().unwrap_or_default();
            let version = format!("{}-{}", build_info.package_ver, build_info.git_commit);
            let output = Self {
                version,
                bootstrap: bootstrap.display().to_string(),
                external_bootstrap: String::new(),
                blobs: blob_ids,
                external_blobs: Vec::new(),
                trace,
                fs_version: fs_version.to_string(),
                compressor: compressor.to_string(),
            };

            serde_json::to_writer(w, &output).context("failed to write result to output file")?;
        }

        Ok(())
    }
}

fn prepare_cmd_args(bti_string: &'static str) -> App {
    let arg_chunk_dict = Arg::new("chunk-dict")
        .long("chunk-dict")
        .help("File path of chunk dictionary for data deduplication");
    let arg_prefetch_policy = Arg::new("prefetch-policy")
        .long("prefetch-policy")
        .help("Set data prefetch policy")
        .required(false)
        .default_value("none")
        .value_parser(["fs", "blob", "none"]);
    let arg_output_json = Arg::new("output-json")
        .long("output-json")
        .short('J')
        .help("File path to save operation result in JSON format");
    let arg_config = Arg::new("config")
        .long("config")
        .short('C')
        .help("Configuration file for storage backend, cache and RAFS FUSE filesystem.")
        .required(false);

    let app = App::new("")
        .version(bti_string)
        .author(crate_authors!())
        .about("Build, analyze, inspect or validate RAFS filesystems/Nydus accelerated container images")
        .arg(
            Arg::new("log-file")
                .long("log-file")
                .short('L')
                .help("Log file path")
                .required(false)
                .global(true),
        )
        .arg(
            Arg::new("log-level")
                .long("log-level")
                .short('l')
                .help("Log level:")
                .default_value("info")
                .value_parser(["trace", "debug", "info", "warn", "error"])
                .required(false)
                .global(true),
        );

    let app = app.subcommand(
            App::new("create")
                .about("Create RAFS filesystems from directories, tar files or OCI images")
                .arg(
                    Arg::new("SOURCE")
                        .help("source from which to build the RAFS filesystem")
                        .required(true)
                        .num_args(1),
                )
                .arg(
                    Arg::new("type")
                        .long("type")
                        .short('t')
                        .alias("source-type")
                        .help("Conversion type:")
                        .default_value("dir-rafs")
                        .value_parser([
                            "directory",
                            "dir-rafs",
                            "estargz-rafs",
                            "estargz-ref",
                            "estargztoc-ref",
                            "tar-rafs",
                            "tar-tarfs",
                            "targz-rafs",
                            "targz-ref",
                            "stargz_index",
                        ])
                )
                .arg(
                    Arg::new("bootstrap")
                        .long("bootstrap")
                        .short('B')
                        .help("File path to save the generated RAFS metadata blob")
                        .required_unless_present_any(["blob-dir", "blob-inline-meta"])
                        .conflicts_with("blob-inline-meta"),
                )
                .arg(
                    Arg::new("blob-dir")
                        .long("blob-dir")
                        .short('D')
                        .help("Directory path to save generated RAFS metadata and data blobs"),
                )
                .arg(
                    Arg::new("blob")
                        .long("blob")
                        .short('b')
                        .help("File path to save the generated RAFS data blob")
                        .required_unless_present_any(["type", "blob-dir"]),
                )
                .arg(
                    Arg::new("external-blob")
                        .long("external-blob")
                        .help("File path to save the generated RAFS external blob")
                        .required_unless_present_any(["type", "blob-dir"]),
                )
                .arg(
                    Arg::new("blob-inline-meta")
                        .long("blob-inline-meta")
                        .alias("inline-bootstrap")
                        .help("Inline RAFS metadata and blob metadata into the data blob")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("blob-id")
                        .required(false),
                )
                .arg(
                    Arg::new("blob-id")
                        .long("blob-id")
                        .required_if_eq_any([("type", "estargztoc-ref"), ("type", "stargz_index")])
                        .help("OSS object id for the generated RAFS data blob")
                )
                .arg(
                    Arg::new("blob-data-size")
                        .long("blob-data-size")
                        .help("Set data blob size for 'estargztoc-ref' conversion"),
                )
                .arg(
                    Arg::new("blob-offset")
                        .long("blob-offset")
                        .help("File offset to store RAFS data, to support storing data blobs into tar files")
                        .hide(true)
                        .default_value("0"),
                )
                .arg(
                    Arg::new("chunk-size")
                        .long("chunk-size")
                        .help("Set the size of data chunks, must be power of two and between 0x1000-0x1000000:")
                        .required(false),
                )
                .arg(
                    Arg::new("batch-size")
                        .long("batch-size")
                        .help("Set the batch size to merge small chunks, must be power of two, between 0x1000-0x1000000 or be zero:")
                        .required(false)
                        .default_value("0"),
                )
                .arg(
                    Arg::new("compressor")
                        .long("compressor")
                        .help("Algorithm to compress data chunks:")
                        .required(false)
                        .default_value("zstd")
                        .value_parser(["none", "lz4_block", "zstd"]),
                )
                .arg(
                    Arg::new("digester")
                        .long("digester")
                        .help("Algorithm to digest data chunks:")
                        .required(false)
                        .default_value("blake3")
                        .value_parser(["blake3", "sha256"]),
                )
                .arg( arg_config.clone() )
                .arg(
                    Arg::new("fs-version")
                        .long("fs-version")
                        .short('v')
                        .help("Set RAFS format version number:")
                        .default_value("6")
                        .value_parser(["5", "6"]),
                )
                .arg(
                    Arg::new("features")
                        .long("features")
                        .value_parser(["blob-toc"])
                        .help("Enable/disable features")
                )
                .arg(
                    arg_chunk_dict.clone(),
                )
                .arg(
                    Arg::new("parent-bootstrap")
                        .long("parent-bootstrap")
                        .help("File path of the parent/referenced RAFS metadata blob (optional)")
                        .required(false),
                )
                .arg(
                    Arg::new("aligned-chunk")
                        .long("aligned-chunk")
                        .help("Align uncompressed data chunks to 4K, only for RAFS V5")
                        .action(ArgAction::SetTrue)
                )
                .arg(
                    Arg::new("repeatable")
                        .long("repeatable")
                        .help("Generate reproducible RAFS metadata")
                        .action(ArgAction::SetTrue)
                        .required(false),
                )
                .arg(
                    Arg::new("disable-check")
                        .long("disable-check")
                        .help("Disable RAFS metadata validation after build")
                        .hide(true)
                        .action(ArgAction::SetTrue)
                        .required(false)
                )
                .arg(
                    Arg::new("whiteout-spec")
                        .long("whiteout-spec")
                        .help("Set the type of whiteout specification:")
                        .default_value("oci")
                        .value_parser(["oci", "overlayfs", "none"])
                )
                .arg(
                    arg_prefetch_policy.clone(),
                )
                .arg(
                    arg_output_json.clone(),
                )
                .arg(
                    Arg::new("encrypt")
                        .long("encrypt")
                        .short('E')
                        .help("Encrypt the generated RAFS metadata and data blobs")
                        .action(ArgAction::SetTrue)
                        .required(false)
                )
                .arg(
                    Arg::new("blob-cache-dir")
                        .long("blob-cache-dir")
                        .help("Directory path to generate blob cache files ($id.blob.meta and $id.blob.data)")
                        .value_parser(clap::value_parser!(PathBuf))
                        .conflicts_with("blob-inline-meta")
                        .conflicts_with("blob")
                        .conflicts_with("blob-dir")
                        .conflicts_with("compressor")
                        .required(false)
                )
                .arg(
                    Arg::new("attributes")
                        .long("attributes")
                        .help("Nydus attributes file path (usually .nydusattributes file)")
                        .value_parser(clap::value_parser!(PathBuf))
                        .required(false)
                )
        );

    let app = app.subcommand(
            App::new("chunkdict")
                .about("deduplicate RAFS filesystem metadata")
                .subcommand(
                    App::new("generate")
                        .about("generate chunk dictionary based on database")
                        .arg(
                            Arg::new("database")
                                .long("database")
                                .help("Database connection address for assisting chunk dictionary generation, e.g. /path/database.db")
                                .default_value("sqlite:///home/runner/output/database.db")
                                .required(false),
                        )
                        .arg(
                            Arg::new("bootstrap")
                                .long("bootstrap")
                                .short('B')
                                .help("Output path of nydus overlaid bootstrap"),
                        )
                        .arg(
                            Arg::new("blob-dir")
                                .long("blob-dir")
                                .short('D')
                                .help("Directory path to save generated RAFS metadata and data blobs"),
                        )
                        .arg(arg_prefetch_policy.clone())
                        .arg(arg_output_json.clone())
                        .arg(arg_config.clone())
                        .arg(
                            Arg::new("SOURCE")
                                .help("bootstrap paths (allow one or more)")
                                .required(true)
                                .num_args(1..),
                        )
                        .arg(
                            Arg::new("verbose")
                                .long("verbose")
                                .short('v')
                                .help("Output message in verbose mode")
                                .action(ArgAction::SetTrue)
                                .required(false),
                        )
                    )
                );

    let app = app.subcommand(
        App::new("merge")
            .about("Merge multiple bootstraps into a overlaid bootstrap")
            .arg(
                Arg::new("parent-bootstrap")
                    .long("parent-bootstrap")
                    .help("File path of the parent/referenced RAFS metadata blob (optional)")
                    .required(false),
            )
            .arg(
                Arg::new("bootstrap")
                    .long("bootstrap")
                    .short('B')
                    .help("Output path of nydus overlaid bootstrap"),
            )
            .arg(
                Arg::new("blob-dir")
                    .long("blob-dir")
                    .short('D')
                    .help("Directory path to save generated RAFS metadata and data blobs"),
            )
            .arg(arg_chunk_dict.clone())
            .arg(arg_prefetch_policy)
            .arg(arg_output_json.clone())
            .arg(
                Arg::new("blob-digests")
                    .long("blob-digests")
                    .required(false)
                    .help("RAFS blob digest list separated by comma"),
            )
            .arg(
                Arg::new("original-blob-ids")
                    .long("original-blob-ids")
                    .required(false)
                    .help("original blob id list separated by comma, it may usually be a sha256 hex string"),
            )
            .arg(
                Arg::new("blob-sizes")
                    .long("blob-sizes")
                    .required(false)
                    .help("RAFS blob size list separated by comma"),
            )
            .arg(
                Arg::new("blob-toc-digests")
                    .long("blob-toc-digests")
                    .required(false)
                    .help("RAFS blob toc digest list separated by comma"),
            )
            .arg(
                Arg::new("blob-toc-sizes")
                    .long("blob-toc-sizes")
                    .required(false)
                    .help("RAFS blob toc size list separated by comma"),
            )
            .arg(arg_config.clone())
            .arg(
                Arg::new("SOURCE")
                    .help("bootstrap paths (allow one or more)")
                    .required(true)
                    .num_args(1..),
            ),
    );

    let app = app.subcommand(
        App::new("check")
            .about("Validate RAFS filesystem metadata")
            .arg(
                Arg::new("BOOTSTRAP")
                    .help("File path of RAFS metadata")
                    .required_unless_present("bootstrap"),
            )
            .arg(
                Arg::new("bootstrap")
                    .short('B')
                    .long("bootstrap")
                    .help("[Deprecated] File path of RAFS meta blob/bootstrap")
                    .conflicts_with("BOOTSTRAP")
                    .required(false),
            )
            .arg(
                Arg::new("blob-dir")
                    .long("blob-dir")
                    .short('D')
                    .conflicts_with("config")
                    .help(
                        "Directory for localfs storage backend, hosting data blobs and cache files",
                    ),
            )
            .arg(arg_config.clone())
            .arg(
                Arg::new("verbose")
                    .long("verbose")
                    .short('v')
                    .help("Output message in verbose mode")
                    .action(ArgAction::SetTrue)
                    .required(false),
            )
            .arg(arg_output_json.clone()),
    );

    let app = app.subcommand(
        App::new("optimize")
            .about("Optimize By Prefetch")
            .arg(
                Arg::new("bootstrap")
                    .help("File path of RAFS metadata")
                    .short('B')
                    .long("bootstrap")
                    .required(true),
            )
            .arg(
                Arg::new("prefetch-files")
                    .long("prefetch-files")
                    .short('p')
                    .help("Path to a hint file listing files to be prefetched, with each file on a separate line.")
                    .action(ArgAction::Set)
                    .num_args(1),
            )
            .arg(arg_config.clone())
            .arg(
                Arg::new("blob-dir")
                    .long("blob-dir")
                    .short('D')
                    .conflicts_with("config")
                    .help(
                        "Directory for localfs storage backend, hosting data blobs and cache files",
                    ),
            )
            .arg(
                Arg::new("output-bootstrap")
                    .long("output-bootstrap")
                    .short('O')
                    .help("Output path of optimized bootstrap"),
            )
            .arg(
                arg_output_json.clone(),
            )
    );

    #[cfg(target_os = "linux")]
    let app = app.subcommand(
            App::new("export")
                .about("Export RAFS filesystems as raw block disk images or tar files")
                .arg(
                    Arg::new("block")
                        .long("block")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Export RAFS filesystems as raw block disk images")
                )
                .arg(
                    Arg::new("bootstrap")
                        .long("bootstrap")
                        .short('B')
                        .help("Bootstrap of the RAFS filesystem to be exported")
                        .requires("localfs-dir")
                )
                .arg(Arg::new("config")
                    .long("config")
                    .short('C')
                    .help("Configuration file containing a `BlobCacheEntry` object")
                    .required(false))
                .arg(
                    Arg::new("localfs-dir")
                        .long("localfs-dir")
                        .short('D')
                        .help(
                            "Path to the `localfs` working directory, which also enables the `localfs` storage backend"
                        )
                        .requires("bootstrap")
                        .conflicts_with("config"),
                )
                .arg(
                    Arg::new("threads")
                        .long("threads")
                        .default_value("4")
                        .help("Number of worker threads to execute export operation, valid values: [1-32]")
                        .value_parser(Command::thread_validator)
                        .required(false),
                )
                .arg(
                    Arg::new("output")
                        .long("output")
                        .short('O')
                        .help("File path for saving the exported content")
                        .required_unless_present("localfs-dir")
                )
                .arg(
                    Arg::new("verity")
                        .long("verity")
                        .help("Generate dm-verity data for block device")
                        .action(ArgAction::SetTrue)
                        .required(false)
                        .requires("block")
                )
        );

    let app = app.subcommand(
        App::new("inspect")
            .about("Inspect RAFS filesystem metadata in interactive or request mode")
            .arg(
                Arg::new("BOOTSTRAP")
                    .help("File path of RAFS metadata")
                    .required_unless_present("bootstrap"),
            )
            .arg(
                Arg::new("bootstrap")
                    .short('B')
                    .long("bootstrap")
                    .help("[Deprecated] File path of RAFS meta blob/bootstrap")
                    .conflicts_with("BOOTSTRAP")
                    .required(false),
            )
            .arg(
                Arg::new("blob-dir")
                    .long("blob-dir")
                    .short('D')
                    .conflicts_with("config")
                    .help(
                        "Directory for localfs storage backend, hosting data blobs and cache files",
                    ),
            )
            .arg(arg_config.clone())
            .arg(
                Arg::new("request")
                    .long("request")
                    .short('R')
                    .help("Inspect RAFS filesystem metadata in request mode")
                    .required(false),
            ),
    );

    let app = app.subcommand(
            App::new("stat")
                .about("Generate statistics information for RAFS filesystems")
                .arg(
                    Arg::new("bootstrap")
                        .long("bootstrap")
                        .short('B')
                        .help("Generate statistics information for the RAFS filesystem")
                        .required(false),
                )
                .arg(
                    Arg::new("blob-dir")
                        .long("blob-dir")
                        .short('D')
                        .help("Generate statistics information for all RAFS filesystems in the directory")
                        .required(false),
                )
                .arg(
                    Arg::new("target")
                        .long("target")
                        .short('T')
                        .help("Generate statistics information for the RAFS filesystem after applying chunk deduplication")
                        .required(false),
                )
                .arg(arg_config.clone())
                .arg(
                    Arg::new("digester")
                        .long("digester")
                        .help("Algorithm to digest data chunks:")
                        .required(false)
                        .default_value("blake3")
                        .value_parser(["blake3", "sha256"]),
                )
                .arg(
                    arg_output_json.clone(),
                )
        );

    let app = app.subcommand(
            App::new("compact")
                .about("(experimental)Compact specific nydus image, remove unused chunks in blobs, merge small blobs")
                .arg(
                    Arg::new("bootstrap")
                        .long("bootstrap")
                        .short('B')
                        .help("bootstrap to compact")
                        .required(true),
                )
                .arg(
                    Arg::new("backend-type")
                        .long("backend-type")
                        .help(format!(
                            "Type of backend [possible values: {}]",
                            BlobFactory::supported_backends()
                                .into_iter()
                                .filter(|x| x != "localfs")
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .required(false)
                )
                .arg(
                    Arg::new("backend-config")
                        .long("backend-config")
                        .help("Config string of backend")
                        .required(false),
                )
                .arg(
                    Arg::new("backend-config-file")
                        .long("backend-config-file")
                        .help("Config file of backend")
                        .conflicts_with("backend-config")
                        .required(false),
                )
                .arg(
                    Arg::new("min-used-ratio")
                        .long("min-used-ratio")
                        .help("Lower bound of used ratio for blobs to be kept, possible values: 0-99, 0 means disable")
                )
                .arg(
                    Arg::new("compact-blob-size")
                        .long("compact-blob-size")
                        .help("Upper bound of blob size for blobs to be compacted, in bytes")
                )
                .arg(
                    Arg::new("max-compact-size")
                        .long("max-compact-size")
                        .help("Upper bound of compacted blob size, in bytes")
                )
                .arg(
                    Arg::new("layers-to-compact")
                        .long("layers-to-compact")
                        .help("If number of blobs >= layers_to_compact, do compact. 0 means always compact")
                )
                .arg(
                    Arg::new("blob-dir")
                        .long("blob-dir")
                        .short('D')
                        .help(
                            "Local blobs dir for blobs not in backend, also for output blobs",
                        )
                        .required(true),
                )
                .arg( arg_chunk_dict )
                .arg(
                    Arg::new("output-bootstrap")
                        .long("output-bootstrap")
                        .short('O')
                        .help("bootstrap to output, default is source bootstrap add suffix .compact"),
                )
                .arg(
                    arg_output_json,
                )
        );

    app.subcommand(
        App::new("unpack")
            .about("Unpack a RAFS filesystem to a tar file")
            .arg(
                Arg::new("BOOTSTRAP")
                    .help("File path of RAFS metadata")
                    .required_unless_present("bootstrap"),
            )
            .arg(
                Arg::new("backend-type")
                    .long("backend-type")
                    .help(format!(
                        "Type of backend [possible values: {}]",
                        BlobFactory::supported_backends()
                            .into_iter()
                            .filter(|x| x != "localfs")
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .required(false)
                    .group("backend"),
            )
            .arg(
                Arg::new("backend-config")
                    .long("backend-config")
                    .help("Config string of backend")
                    .required(false),
            )
            .arg(
                Arg::new("backend-config-file")
                    .long("backend-config-file")
                    .help("Config file of backend")
                    .conflicts_with("backend-config")
                    .required(false),
            )
            .arg(
                Arg::new("bootstrap")
                    .short('B')
                    .long("bootstrap")
                    .help("[Deprecated] File path of RAFS meta blob/bootstrap")
                    .conflicts_with("BOOTSTRAP")
                    .required(false),
            )
            .arg(
                Arg::new("blob")
                    .long("blob")
                    .short('b')
                    .help("Path to RAFS data blob file")
                    .required(false)
                    .group("backend"),
            )
            .arg(
                Arg::new("blob-dir")
                    .long("blob-dir")
                    .short('D')
                    .help(
                        "Directory for localfs storage backend, hosting data blobs and cache files",
                    )
                    .group("backend"),
            )
            .arg(
                Arg::new("output")
                    .long("output")
                    .help("Path for output tar file")
                    .required(true),
            )
            .group(
                clap::ArgGroup::new("backend")
                    .args(["backend-type", "blob", "blob-dir"])
                    .required(false),
            ),
    )
}

fn init_log(matches: &ArgMatches) -> Result<()> {
    let mut log_file = None;
    if let Some(file) = matches.get_one::<String>("log-file") {
        let path = PathBuf::from(file);
        log_file = Some(path);
    }

    // Safe to unwrap because it has a default value and possible values are defined.
    let level = matches
        .get_one::<String>("log-level")
        .unwrap()
        .parse()
        .unwrap();

    setup_logging(log_file, level, 0).context("failed to setup logging")
}

lazy_static! {
    static ref BTI_STRING: String = get_build_time_info().0;
    static ref BTI: BuildTimeInfo = get_build_time_info().1;
}

fn main() -> Result<()> {
    let build_info = BTI.to_owned();
    let mut app = prepare_cmd_args(BTI_STRING.as_str());
    let usage = app.render_usage();
    let cmd = app.get_matches();

    init_log(&cmd)?;

    register_tracer!(TraceClass::Timing, TimingTracerClass);
    register_tracer!(TraceClass::Event, EventTracerClass);

    if let Some(matches) = cmd.subcommand_matches("create") {
        Command::create(matches, &build_info)
    } else if let Some(matches) = cmd.subcommand_matches("chunkdict") {
        match matches.subcommand_name() {
            Some("generate") => Command::chunkdict_generate(
                matches.subcommand_matches("generate").unwrap(),
                &build_info,
            ),
            _ => {
                println!("{}", usage);
                Ok(())
            }
        }
    } else if let Some(matches) = cmd.subcommand_matches("merge") {
        let result = Command::merge(matches, &build_info);
        if let Err(ref err) = result {
            if let Some(MergeError::InconsistentFilesystem(_)) = err.downcast_ref::<MergeError>() {
                error!("message:{}", err);
                std::process::exit(2);
            }
        }
        result
    } else if let Some(matches) = cmd.subcommand_matches("check") {
        Command::check(matches, &build_info)
    } else if let Some(matches) = cmd.subcommand_matches("inspect") {
        Command::inspect(matches)
    } else if let Some(matches) = cmd.subcommand_matches("stat") {
        Command::stat(matches)
    } else if let Some(matches) = cmd.subcommand_matches("compact") {
        Command::compact(matches, &build_info)
    } else if let Some(matches) = cmd.subcommand_matches("unpack") {
        Command::unpack(matches)
    } else if let Some(matches) = cmd.subcommand_matches("optimize") {
        Command::optimize(matches, &build_info)
    } else {
        #[cfg(target_os = "linux")]
        if let Some(matches) = cmd.subcommand_matches("export") {
            Command::export(&cmd, matches, &build_info)
        } else {
            println!("{}", usage);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            println!("{}", usage);
            Ok(())
        }
    }
}

struct Command {}

impl Command {
    fn create(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let blob_id = Self::get_blob_id(matches)?;
        let blob_offset = Self::get_blob_offset(matches)?;
        let parent_path = Self::get_parent_bootstrap(matches)?;
        let prefetch = Self::get_prefetch(matches)?;
        let source_path = PathBuf::from(matches.get_one::<String>("SOURCE").unwrap());
        let conversion_type: ConversionType = matches.get_one::<String>("type").unwrap().parse()?;
        let blob_inline_meta = matches.get_flag("blob-inline-meta");
        let repeatable = matches.get_flag("repeatable");
        let version = Self::get_fs_version(matches)?;
        let chunk_size = Self::get_chunk_size(matches, conversion_type)?;
        let batch_size = Self::get_batch_size(matches, version, conversion_type, chunk_size)?;
        let blob_cache_storage = Self::get_blob_cache_storage(matches, conversion_type)?;
        // blob-cache-dir and blob-dir/blob are a set of mutually exclusive functions,
        // the former is used to generate blob cache, nydusd is directly started through blob cache,
        // the latter is to generate nydus blob, as nydusd backend to start
        let blob_storage = if blob_cache_storage.is_some() {
            None
        } else {
            Self::get_blob_storage(matches, conversion_type)?
        };

        let aligned_chunk = if version.is_v6() && conversion_type != ConversionType::TarToTarfs {
            true
        } else {
            // get_fs_version makes sure it's either v6 or v5.
            matches.get_flag("aligned-chunk")
        };
        let whiteout_spec: WhiteoutSpec = matches
            .get_one::<String>("whiteout-spec")
            .map(|s| s.as_str())
            .unwrap_or_default()
            .parse()?;
        let mut compressor = matches
            .get_one::<String>("compressor")
            .map(|s| s.as_str())
            .unwrap_or_default()
            .parse()?;
        let mut digester = matches
            .get_one::<String>("digester")
            .map(|s| s.as_str())
            .unwrap_or_default()
            .parse()?;
        let blob_data_size = Self::get_blob_size(matches, conversion_type)?;
        let features = Features::try_from(
            matches
                .get_one::<String>("features")
                .map(|s| s.as_str())
                .unwrap_or_default(),
        )?;
        let encrypt = matches.get_flag("encrypt");
        match conversion_type {
            ConversionType::DirectoryToRafs => {
                Self::ensure_directory(&source_path)?;
                if blob_storage.is_none() && blob_cache_storage.is_none() {
                    bail!("both --blob and --blob-dir or --blob-cache-dir are missing");
                }
            }
            ConversionType::EStargzToRafs
            | ConversionType::TargzToRafs
            | ConversionType::TarToRafs => {
                Self::ensure_file(&source_path)?;
                if blob_storage.is_none() && blob_cache_storage.is_none() {
                    bail!("both --blob and --blob-dir or --blob-cache-dir are missing");
                }
            }
            ConversionType::TarToRef
            | ConversionType::TargzToRef
            | ConversionType::EStargzToRef => {
                Self::ensure_file(&source_path)?;
                if matches.value_source("compressor") != Some(ValueSource::DefaultValue)
                    && compressor != compress::Algorithm::GZip
                {
                    info!(
                        "only GZip is supported for conversion type {}, use GZip instead of {}",
                        conversion_type, compressor
                    );
                }
                if matches.value_source("digester") != Some(ValueSource::DefaultValue)
                    && digester != digest::Algorithm::Sha256
                {
                    info!(
                        "only SHA256 is supported for conversion type {}, use SHA256 instead of {}",
                        conversion_type, compressor
                    );
                }
                compressor = compress::Algorithm::GZip;
                digester = digest::Algorithm::Sha256;
                if blob_storage.is_none() && blob_cache_storage.is_none() {
                    bail!("all of --blob, --blob-dir and --blob-cache-dir are missing");
                } else if !prefetch.disabled && prefetch.policy == PrefetchPolicy::Blob {
                    bail!(
                        "conversion type {} conflicts with '--prefetch-policy blob'",
                        conversion_type
                    );
                }
                if version != RafsVersion::V6 {
                    bail!(
                        "'--fs-version 5' conflicts with conversion type '{}', only V6 is supported",
                        conversion_type
                    );
                }
                if blob_id.trim() != "" {
                    bail!(
                        "conversion type '{}' conflicts with '--blob-id'",
                        conversion_type
                    );
                }
                if encrypt {
                    bail!(
                        "conversion type '{}' conflicts with '--encrypt'",
                        conversion_type
                    )
                }
            }
            ConversionType::TarToTarfs => {
                Self::ensure_file(&source_path)?;
                if matches.value_source("compressor") != Some(ValueSource::DefaultValue)
                    && compressor != compress::Algorithm::None
                {
                    info!(
                        "only compressor `None` is supported for conversion type {}, use `None` instead of {}",
                        conversion_type, compressor
                    );
                }
                if matches.value_source("digester") != Some(ValueSource::DefaultValue)
                    && digester != digest::Algorithm::Sha256
                {
                    info!(
                        "only SHA256 is supported for conversion type {}, use SHA256 instead of {}",
                        conversion_type, compressor
                    );
                }
                compressor = compress::Algorithm::None;
                digester = digest::Algorithm::Sha256;
                if blob_storage.is_none() && blob_cache_storage.is_none() {
                    bail!("both --blob and --blob-dir or --blob-cache-dir are missing");
                } else if !prefetch.disabled && prefetch.policy == PrefetchPolicy::Blob {
                    bail!(
                        "conversion type {} conflicts with '--prefetch-policy blob'",
                        conversion_type
                    );
                }
                if version != RafsVersion::V6 {
                    bail!(
                        "'--fs-version 5' conflicts with conversion type '{}', only V6 is supported",
                        conversion_type
                    );
                }
                if matches.get_one::<String>("chunk-dict").is_some() {
                    bail!(
                        "conversion type '{}' conflicts with '--chunk-dict'",
                        conversion_type
                    );
                }
                if parent_path.is_some() {
                    bail!(
                        "conversion type '{}' conflicts with '--parent-bootstrap'",
                        conversion_type
                    );
                }
                if blob_inline_meta {
                    bail!(
                        "conversion type '{}' conflicts with '--blob-inline-meta'",
                        conversion_type
                    );
                }
                if features.is_enabled(Feature::BlobToc) {
                    bail!(
                        "conversion type '{}' conflicts with '--features blob-toc'",
                        conversion_type
                    );
                }
                if aligned_chunk {
                    bail!(
                        "conversion type '{}' conflicts with '--aligned-chunk'",
                        conversion_type
                    );
                }
                if encrypt {
                    bail!(
                        "conversion type '{}' conflicts with '--encrypt'",
                        conversion_type
                    )
                }
            }
            ConversionType::EStargzIndexToRef => {
                Self::ensure_file(&source_path)?;
                if matches.value_source("compressor") != Some(ValueSource::DefaultValue)
                    && compressor != compress::Algorithm::GZip
                {
                    info!(
                        "only GZip is supported for conversion type {}, use GZip instead of {}",
                        conversion_type, compressor
                    );
                }
                if matches.value_source("digester") != Some(ValueSource::DefaultValue)
                    && digester != digest::Algorithm::Sha256
                {
                    info!(
                        "only SHA256 is supported for conversion type {}, use SHA256 instead of {}",
                        conversion_type, compressor
                    );
                }
                compressor = compress::Algorithm::GZip;
                digester = digest::Algorithm::Sha256;
                if blob_storage.is_some() || blob_cache_storage.is_some() {
                    bail!(
                        "conversion type '{}' conflicts with '--blob' and '--blob-cache-dir'",
                        conversion_type
                    );
                }
                if version != RafsVersion::V6 {
                    bail!(
                        "'--fs-version 5' conflicts with conversion type '{}', only V6 is supported",
                        conversion_type
                    );
                }
                if blob_id.trim() == "" {
                    bail!("'--blob-id' is missing for '--type stargz_index'");
                }
                if encrypt {
                    bail!(
                        "conversion type '{}' conflicts with '--encrypt'",
                        conversion_type
                    )
                }
            }
            ConversionType::DirectoryToStargz
            | ConversionType::TargzToStargz
            | ConversionType::TarToStargz => {
                unimplemented!()
            }
            ConversionType::DirectoryToTargz => {
                unimplemented!()
            }
        }

        if features.is_enabled(Feature::BlobToc) && version == RafsVersion::V5 {
            bail!("`--features blob-toc` can't be used with `--version 5` ");
        }

        if blob_cache_storage.is_some() {
            // In blob cache mode, we don't need to do any compression for the original data
            compressor = compress::Algorithm::None;
        }

        let attributes = matches
            .get_one::<PathBuf>("attributes")
            .map(Attributes::from)
            .transpose()?
            .unwrap_or_default();
        let external_blob_storage = matches
            .get_one::<String>("external-blob")
            .map(|b| ArtifactStorage::SingleFile(b.into()));
        let mut build_ctx = BuildContext::new(
            blob_id,
            aligned_chunk,
            blob_offset,
            compressor,
            digester,
            !repeatable,
            whiteout_spec,
            conversion_type,
            source_path,
            prefetch,
            blob_storage,
            external_blob_storage,
            blob_inline_meta,
            features,
            encrypt,
            attributes,
        );
        build_ctx.set_fs_version(version);
        build_ctx.set_chunk_size(chunk_size);
        build_ctx.set_batch_size(batch_size);

        let blob_cache_generator = match blob_cache_storage {
            Some(storage) => Some(BlobCacheGenerator::new(storage)?),
            None => None,
        };
        build_ctx.blob_cache_generator = blob_cache_generator;

        let mut config = Self::get_configuration(matches)?;
        if let Some(cache) = Arc::get_mut(&mut config).unwrap().cache.as_mut() {
            cache.cache_validate = true;
        }
        config.internal.set_blob_accessible(true);
        build_ctx.set_configuration(config.clone());

        let mut blob_mgr = BlobManager::new(digester, false);
        if let Some(chunk_dict_arg) = matches.get_one::<String>("chunk-dict") {
            let config = RafsSuperConfig {
                version,
                compressor,
                digester,
                chunk_size,
                batch_size,
                explicit_uidgid: !repeatable,
                is_tarfs_mode: false,
            };
            let rafs_config = Arc::new(build_ctx.configuration.as_ref().clone());
            // The separate chunk dict bootstrap doesn't support blob accessible.
            rafs_config.internal.set_blob_accessible(false);
            blob_mgr.set_chunk_dict(timing_tracer!(
                { HashChunkDict::from_commandline_arg(chunk_dict_arg, rafs_config, &config,) },
                "import_chunk_dict"
            )?);
        }

        let mut bootstrap_mgr = if blob_inline_meta {
            BootstrapManager::new(None, parent_path)
        } else {
            let bootstrap_path = Self::get_bootstrap_storage(matches)?;
            BootstrapManager::new(Some(bootstrap_path), parent_path)
        };

        // Legality has been checked and filtered by `get_batch_size()`.
        if build_ctx.batch_size > 0 {
            let generator = BatchContextGenerator::new(build_ctx.batch_size)?;
            build_ctx.blob_batch_generator = Some(Mutex::new(generator));
            build_ctx.blob_features.insert(BlobFeatures::BATCH);
            build_ctx.blob_features.insert(BlobFeatures::CHUNK_INFO_V2);
        }

        let mut builder: Box<dyn Builder> = match conversion_type {
            ConversionType::DirectoryToRafs => {
                if encrypt {
                    build_ctx.blob_features.insert(BlobFeatures::CHUNK_INFO_V2);
                    build_ctx.blob_features.insert(BlobFeatures::ENCRYPTED);
                }
                Box::new(DirectoryBuilder::new())
            }
            ConversionType::EStargzIndexToRef => {
                Box::new(StargzBuilder::new(blob_data_size, &build_ctx))
            }
            ConversionType::EStargzToRafs
            | ConversionType::TargzToRafs
            | ConversionType::TarToRafs => {
                if encrypt {
                    build_ctx.blob_features.insert(BlobFeatures::CHUNK_INFO_V2);
                    build_ctx.blob_features.insert(BlobFeatures::ENCRYPTED);
                }
                Box::new(TarballBuilder::new(conversion_type))
            }
            ConversionType::EStargzToRef
            | ConversionType::TargzToRef
            | ConversionType::TarToRef => {
                if version.is_v5() {
                    bail!("conversion type {} conflicts with RAFS v5", conversion_type);
                }
                build_ctx.blob_features.insert(BlobFeatures::CHUNK_INFO_V2);
                build_ctx.blob_features.insert(BlobFeatures::SEPARATE);
                Box::new(TarballBuilder::new(conversion_type))
            }
            ConversionType::TarToTarfs => {
                if version.is_v5() {
                    bail!("conversion type {} conflicts with RAFS v5", conversion_type);
                }
                Box::new(TarballBuilder::new(conversion_type))
            }
            ConversionType::DirectoryToStargz
            | ConversionType::DirectoryToTargz
            | ConversionType::TarToStargz
            | ConversionType::TargzToStargz => unimplemented!(),
        };
        let build_output = timing_tracer!(
            {
                builder
                    .build(&mut build_ctx, &mut bootstrap_mgr, &mut blob_mgr)
                    .context("build failed")
            },
            "total_build"
        )?;

        lazy_drop(build_ctx);

        // Some operations like listing xattr pairs of certain namespace need the process
        // to be privileged. Therefore, trace what euid and egid are.
        event_tracer!("euid", "{}", geteuid());
        event_tracer!("egid", "{}", getegid());
        info!("successfully built RAFS filesystem: \n{}", build_output);
        OutputSerializer::dump(matches, build_output, build_info, compressor, version)
    }

    fn chunkdict_generate(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let mut build_ctx = BuildContext {
            prefetch: Self::get_prefetch(matches)?,
            ..Default::default()
        };
        let db_url: &String = matches.get_one::<String>("database").unwrap();
        // Save chunk and blob info to database.
        let source_bootstrap_paths: Vec<PathBuf> = matches
            .get_many::<String>("SOURCE")
            .map(|paths| paths.map(PathBuf::from).collect())
            .unwrap();

        check_bootstrap_versions_consistency(&mut build_ctx, &source_bootstrap_paths)?;
        update_ctx_from_parent_bootstrap(&mut build_ctx, &source_bootstrap_paths[0])?;

        for bootstrap_path in source_bootstrap_paths.iter() {
            let path_name = bootstrap_path.as_path();

            // Extract the image name and version name from the bootstrap directory.
            let bootstrap_dir = match path_name
                .parent()
                .and_then(|p| p.file_name().and_then(|f| f.to_str()))
            {
                Some(dir_str) => dir_str.to_string(),
                None => bail!("Invalid Bootstrap directory name"),
            };
            let full_image_name: Vec<&str> = bootstrap_dir.split(':').collect();
            let image_name = match full_image_name.get(full_image_name.len() - 2) {
                Some(&second_last) => second_last.to_string(),
                None => bail!(
                    "Invalid image name {:?}",
                    full_image_name.get(full_image_name.len() - 2)
                ),
            };
            let image_tag = match full_image_name.last() {
                Some(&last) => last.to_string(),
                None => bail!("Invalid version name {:?}", full_image_name.last()),
            };
            // For backward compatibility with v2.1.
            let config = Self::get_configuration(matches)?;
            config
                .internal
                .set_blob_accessible(matches.get_one::<String>("bootstrap").is_none());
            let db_strs: Vec<&str> = db_url.split("://").collect();
            if db_strs.len() != 2 || (!db_strs[1].starts_with('/') && !db_strs[1].starts_with(':'))
            {
                bail!("Invalid database URL: {}", db_url);
            }
            match db_strs[0] {
                "sqlite" => {
                    let mut deduplicate: Deduplicate<SqliteDatabase> =
                        Deduplicate::<SqliteDatabase>::new(db_strs[1])?;
                    deduplicate.save_metadata(bootstrap_path, config, image_name, image_tag)?
                }
                _ => {
                    bail!("Unsupported database type: {}, please use a valid database URI, such as 'sqlite:///path/to/chunkdict.db'.", db_strs[0])
                }
            };
        }
        info!("Chunkdict metadata is saved at: {:?}", db_url);

        // Connecting database and generating chunk dictionary by algorithm "exponential_smoothing".
        let db_strs: Vec<&str> = db_url.split("://").collect();
        if db_strs.len() != 2 || (!db_strs[1].starts_with('/') && !db_strs[1].starts_with(':')) {
            bail!("Invalid database URL: {}", db_url);
        }
        let algorithm = String::from("exponential_smoothing");
        let _source_bootstrap_paths: Vec<PathBuf> = matches
            .get_many::<String>("SOURCE")
            .map(|paths| paths.map(PathBuf::from).collect())
            .unwrap();

        let (chunkdict_chunks, chunkdict_blobs, noise_points): (
            Vec<ChunkdictChunkInfo>,
            Vec<ChunkdictBlobInfo>,
            Vec<String>,
        );

        match db_strs[0] {
            "sqlite" => {
                let mut algorithm: deduplicate::Algorithm<SqliteDatabase> =
                    deduplicate::Algorithm::<SqliteDatabase>::new(algorithm, db_strs[1])?;
                let result = algorithm.chunkdict_generate()?;
                chunkdict_chunks = result.0;
                chunkdict_blobs = result.1;
                noise_points = result.2;
            }
            _ => {
                bail!("Unsupported database type: {}, please use a valid database URI, such as 'sqlite:///path/to/chunkdict.db'.", db_strs[0])
            }
        };

        // Output noise point in DBSCAN clustering algorithm.
        info!(
            "The length of chunkdict is {}",
            Vec::<ChunkdictChunkInfo>::len(&chunkdict_chunks)
        );
        info!("It is not recommended to use image deduplication");
        for image_name in noise_points {
            info!("{}", image_name);
        }

        // Dump chunkdict to bootstrap.
        let chunkdict_bootstrap_path = Self::get_bootstrap_storage(matches)?;
        let config =
            Self::get_configuration(matches).context("failed to get configuration information")?;
        config
            .internal
            .set_blob_accessible(matches.get_one::<String>("config").is_some());
        build_ctx.configuration = config;
        build_ctx.blob_storage = Some(chunkdict_bootstrap_path);
        build_ctx
            .blob_features
            .insert(BlobFeatures::IS_CHUNKDICT_GENERATED);
        build_ctx.is_chunkdict_generated = true;

        let mut blob_mgr = BlobManager::new(build_ctx.digester, false);

        let bootstrap_path = Self::get_bootstrap_storage(matches)?;
        let mut bootstrap_mgr = BootstrapManager::new(Some(bootstrap_path), None);

        let output = Generator::generate(
            &mut build_ctx,
            &mut bootstrap_mgr,
            &mut blob_mgr,
            chunkdict_chunks,
            chunkdict_blobs,
        )?;
        OutputSerializer::dump(
            matches,
            output,
            build_info,
            build_ctx.compressor,
            build_ctx.fs_version,
        )
        .unwrap();
        info!(
            "Chunkdict metadata is saved at: {:?}",
            matches
                .get_one::<String>("bootstrap")
                .map(|s| s.as_str())
                .unwrap_or_default(),
        );

        Ok(())
    }

    fn merge(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let source_bootstrap_paths: Vec<PathBuf> = matches
            .get_many::<String>("SOURCE")
            .map(|paths| paths.map(PathBuf::from).collect())
            .unwrap();
        let blob_sizes: Option<Vec<u64>> = matches.get_one::<String>("blob-sizes").map(|list| {
            list.split(',')
                .map(|item| {
                    item.trim()
                        .parse::<u64>()
                        .expect("invalid number in --blob-sizes option")
                })
                .collect()
        });
        let blob_digests: Option<Vec<String>> =
            matches.get_one::<String>("blob-digests").map(|list| {
                list.split(',')
                    .map(|item| item.trim().to_string())
                    .collect()
            });
        let original_blob_ids: Option<Vec<String>> =
            matches.get_one::<String>("original-blob-ids").map(|list| {
                list.split(',')
                    .map(|item| item.trim().to_string())
                    .collect()
            });
        let blob_toc_sizes: Option<Vec<u64>> =
            matches.get_one::<String>("blob-toc-sizes").map(|list| {
                list.split(',')
                    .map(|item| {
                        item.trim()
                            .parse::<u64>()
                            .expect("invalid number in --blob-toc-sizes option")
                    })
                    .collect()
            });
        let blob_toc_digests: Option<Vec<String>> =
            matches.get_one::<String>("blob-toc-digests").map(|list| {
                list.split(',')
                    .map(|item| item.trim().to_string())
                    .collect()
            });
        let target_bootstrap_path = Self::get_bootstrap_storage(matches)?;
        let chunk_dict_path = if let Some(arg) = matches.get_one::<String>("chunk-dict") {
            Some(parse_chunk_dict_arg(arg)?)
        } else {
            None
        };
        let config =
            Self::get_configuration(matches).context("failed to get configuration information")?;
        config
            .internal
            .set_blob_accessible(matches.get_one::<String>("config").is_some());
        let mut ctx = BuildContext {
            prefetch: Self::get_prefetch(matches)?,
            ..Default::default()
        };
        ctx.configuration = config.clone();

        let parent_bootstrap_path = Self::get_parent_bootstrap(matches)?;
        let meta = RafsSuper::load_from_file(&source_bootstrap_paths[0], config.clone(), false)?
            .0
            .meta;

        let output = Merger::merge(
            &mut ctx,
            parent_bootstrap_path,
            source_bootstrap_paths,
            blob_digests,
            original_blob_ids,
            blob_sizes,
            blob_toc_digests,
            blob_toc_sizes,
            target_bootstrap_path,
            chunk_dict_path,
            config,
        )?;
        OutputSerializer::dump(
            matches,
            output,
            build_info,
            meta.get_compressor(),
            meta.version.try_into().unwrap(),
        )
    }

    fn compact(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let bootstrap_path = PathBuf::from(Self::get_bootstrap(matches)?);
        let dst_bootstrap = match matches.get_one::<String>("output-bootstrap") {
            None => bootstrap_path.with_extension("bootstrap.compact"),
            Some(s) => PathBuf::from(s),
        };

        let (config, backend) = match Self::get_backend(matches, "compactor") {
            Ok((c, b)) => (c, b),
            Err(e) => {
                bail!("{}, --blob-dir or --backend-type must be specified", e);
            }
        };

        let (rs, _) = RafsSuper::load_from_file(&bootstrap_path, config.clone(), false)?;
        info!("load bootstrap {:?} successfully", bootstrap_path);
        let chunk_dict = match matches.get_one::<String>("chunk-dict") {
            None => None,
            Some(args) => Some(HashChunkDict::from_commandline_arg(
                args,
                config,
                &rs.meta.get_config(),
            )?),
        };

        let mut compact_config: nydus_builder::CompactConfig = Default::default();
        if let Some(min_used_ratio) = matches.get_one::<String>("min-used-ratio") {
            compact_config.min_used_ratio = min_used_ratio.parse()?;
        }
        if let Some(compact_blob_size) = matches.get_one::<String>("compact-blob-size") {
            compact_config.compact_blob_size = compact_blob_size.parse()?;
        }
        if let Some(max_compact_size) = matches.get_one::<String>("max-compact-size") {
            compact_config.max_compact_size = max_compact_size.parse()?;
        }
        if let Some(layers_to_compact) = matches.get_one::<String>("layers-to-compact") {
            compact_config.layers_to_compact = layers_to_compact.parse()?;
        }

        compact_config.blobs_dir = matches.get_one::<String>("blob-dir").unwrap().clone();

        let version = rs.meta.version.try_into().unwrap();
        let compressor = rs.meta.get_compressor();
        if let Some(build_output) =
            BlobCompactor::compact(rs, dst_bootstrap, chunk_dict, backend, &compact_config)?
        {
            OutputSerializer::dump(matches, build_output, build_info, compressor, version)?;
        }
        Ok(())
    }

    fn unpack(matches: &ArgMatches) -> Result<()> {
        let bootstrap = Self::get_bootstrap(matches)?;
        let output = matches.get_one::<String>("output").expect("pass in output");
        if output.is_empty() {
            return Err(anyhow!("invalid empty --output option"));
        }

        let (config, backend): (Arc<ConfigV2>, Arc<dyn BlobBackend + Send + Sync>) =
            // if --blob is specified, use localfs backend and default config
            if let Some(p) = matches.get_one::<String>("blob") {
                let blob_path = PathBuf::from(p);
                let local_fs_conf = LocalFsConfig {
                    blob_file: blob_path.to_str().unwrap().to_owned(),
                    dir: Default::default(),
                    alt_dirs: Default::default(),
                };
                let local_fs = LocalFs::new(&local_fs_conf, Some("unpacker"))
                    .with_context(|| format!("fail to create local backend for {:?}", blob_path))?;

                (Arc::new(ConfigV2::default()), Arc::new(local_fs))
            } else {
                match Self::get_backend(matches, "unpacker") {
                    Ok((c, b)) => (c, b),
                    Err(e) => {
                        bail!(
                            "{}, --blob, --blob-dir or --backend-type must be specified",
                            e
                        );
                    }
                }
            };

        OCIUnpacker::new(bootstrap, Some(backend), output)
            .with_context(|| "fail to create unpacker")?
            .unpack(config)
            .with_context(|| "fail to unpack")
    }

    fn check(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let bootstrap_path = Self::get_bootstrap(matches)?;
        let verbose = matches.get_flag("verbose");
        let config = Self::get_configuration(matches)?;
        // For backward compatibility with v2.1
        config
            .internal
            .set_blob_accessible(matches.get_one::<String>("bootstrap").is_none());

        let mut validator = Validator::new(bootstrap_path, config)?;
        let (blobs, compressor, fs_version) = validator
            .check(verbose)
            .with_context(|| format!("failed to check bootstrap {:?}", bootstrap_path))?;

        println!("RAFS filesystem metadata is valid, referenced data blobs: ");
        let mut blob_ids = Vec::new();
        for (idx, blob) in blobs.iter().enumerate() {
            println!(
                "\t {}: {}, compressed size {}, uncompressed size {}, chunk size: {}, chunk count: {}, features: {}",
                idx,
                blob.blob_id(),
                blob.compressed_size(),
                blob.uncompressed_size(),
                blob.chunk_size(),
                blob.chunk_count(),
                format_blob_features(blob.features()),
            );
            blob_ids.push(blob.blob_id().to_string());
        }

        OutputSerializer::dump_for_check(
            matches,
            build_info,
            blob_ids,
            bootstrap_path,
            compressor,
            fs_version,
        )?;

        Ok(())
    }

    fn optimize(matches: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let blobs_dir_path = Self::get_blobs_dir(matches)?;
        let prefetch_files = Self::get_prefetch_files(matches)?;
        prefetch_files.iter().for_each(|f| println!("{}", f));
        let bootstrap_path = Self::get_bootstrap(matches)?;
        let dst_bootstrap = match matches.get_one::<String>("output-bootstrap") {
            None => ArtifactStorage::SingleFile(PathBuf::from("optimized_bootstrap")),
            Some(s) => ArtifactStorage::SingleFile(PathBuf::from(s)),
        };

        let config = Self::get_configuration(matches)?;
        config.internal.set_blob_accessible(true);
        let mut build_ctx = BuildContext {
            blob_id: String::from("prefetch-blob"),
            blob_inline_meta: true,
            ..Default::default()
        };

        let sb = update_ctx_from_bootstrap(&mut build_ctx, config, bootstrap_path)?;
        let mut tree = Tree::from_bootstrap(&sb, &mut ())?;

        let mut prefetch_nodes: Vec<TreeNode> = Vec::new();
        // Init prefetch nodes
        for f in prefetch_files.iter() {
            let path = PathBuf::from(f);
            if let Some(tree) = tree.get_node(&path) {
                prefetch_nodes.push(tree.node.clone());
            }
        }

        let mut bootstrap_mgr = BootstrapManager::new(Some(dst_bootstrap), None);
        let blobs = sb.superblock.get_blob_infos();

        let mut blob_table = match build_ctx.fs_version {
            RafsVersion::V5 => RafsBlobTable::V5(RafsV5BlobTable {
                entries: blobs,
                extended: RafsV5ExtBlobTable::new(),
            }),

            RafsVersion::V6 => RafsBlobTable::V6(RafsV6BlobTable { entries: blobs }),
        };
        let output = OptimizePrefetch::generate_prefetch(
            &mut tree,
            &mut build_ctx,
            &mut bootstrap_mgr,
            &mut blob_table,
            blobs_dir_path.to_path_buf(),
            prefetch_nodes,
        )
        .with_context(|| "Failed to generate prefetch bootstrap")?;

        OutputSerializer::dump(
            matches,
            output,
            build_info,
            build_ctx.compressor,
            build_ctx.fs_version,
        )
    }

    fn inspect(matches: &ArgMatches) -> Result<()> {
        let bootstrap_path = Self::get_bootstrap(matches)?;
        let mut config = Self::get_configuration(matches)?;
        // For backward compatibility with v2.1
        config
            .internal
            .set_blob_accessible(matches.get_one::<String>("bootstrap").is_none());
        if let Some(cache) = Arc::get_mut(&mut config).unwrap().cache.as_mut() {
            cache.cache_validate = true;
        }

        let cmd = matches.get_one::<String>("request");
        let mut inspector = inspect::RafsInspector::new(bootstrap_path, cmd.is_some(), config)
            .map_err(|e| {
                error!("failed to create inspector, {:?}", e);
                e
            })?;

        if let Some(c) = cmd {
            let o = inspect::Executor::execute(&mut inspector, c.to_string()).unwrap();
            serde_json::to_writer(std::io::stdout(), &o)
                .unwrap_or_else(|e| error!("Failed to serialize result, {:?}", e));
        } else {
            inspect::Prompt::run(inspector);
        }

        Ok(())
    }

    fn stat(matches: &ArgMatches) -> Result<()> {
        let digester = matches
            .get_one::<String>("digester")
            .map(|s| s.as_str())
            .unwrap_or_default()
            .parse()?;
        let mut stat = stat::ImageStat::new(digester);
        let target = matches
            .get_one::<String>("target")
            .map(Path::new)
            .unwrap_or_else(|| Path::new(""));
        let mut config = Self::get_configuration(matches)?;
        if let Some(cache) = Arc::get_mut(&mut config).unwrap().cache.as_mut() {
            cache.cache_validate = true;
        }
        config
            .internal
            .set_blob_accessible(matches.get_one::<String>("config").is_some());

        if let Some(blob) = matches.get_one::<String>("bootstrap").map(PathBuf::from) {
            stat.stat(&blob, true, config.clone())?;
        } else if let Some(d) = matches.get_one::<String>("blob-dir").map(PathBuf::from) {
            Self::ensure_directory(d.clone())?;

            stat.dedup_enabled = true;

            let children = fs::read_dir(d.as_path())
                .with_context(|| format!("failed to read dir {:?}", d.as_path()))?;
            let children = children.collect::<Result<Vec<DirEntry>, std::io::Error>>()?;
            for child in children {
                let path = child.path();
                if path.is_file() && path != target && path.extension().is_none() {
                    if let Err(e) = stat.stat(&path, true, config.clone()) {
                        debug!(
                            "failed to process {}, {}",
                            path.to_str().unwrap_or_default(),
                            e
                        );
                    };
                }
            }
        } else {
            bail!("one of `--bootstrap` and `--blob-dir` must be specified");
        }

        if let Some(blob) = matches.get_one::<String>("target").map(PathBuf::from) {
            stat.target_enabled = true;
            stat.stat(&blob, false, config)?;
        }

        stat.finalize();

        if let Some(path) = matches.get_one::<String>("output-json").map(PathBuf::from) {
            stat.dump_json(&path)?;
        } else {
            stat.dump();
        }

        Ok(())
    }

    fn get_bootstrap(matches: &ArgMatches) -> Result<&Path> {
        match matches.get_one::<String>("bootstrap") {
            Some(s) => Ok(Path::new(s)),
            None => match matches.get_one::<String>("BOOTSTRAP") {
                Some(s) => Ok(Path::new(s)),
                None => bail!("missing parameter `bootstrap` or `BOOTSTRAP`"),
            },
        }
    }

    fn get_blobs_dir(matches: &ArgMatches) -> Result<&Path> {
        match matches.get_one::<String>("blob-dir") {
            Some(s) => Ok(Path::new(s)),
            None => bail!("missing parameter `blob-dir`"),
        }
    }

    fn get_prefetch_files(matches: &ArgMatches) -> Result<Vec<String>> {
        match matches.get_one::<String>("prefetch-files") {
            Some(v) => {
                let content = std::fs::read_to_string(v)
                    .map_err(|e| anyhow!("failed to read prefetch files from {}: {}", v, e))?;

                let mut prefetch_files: Vec<String> = Vec::new();
                for line in content.lines() {
                    if line.is_empty() || line.trim().is_empty() {
                        continue;
                    }
                    prefetch_files.push(line.trim().to_string());
                }
                Ok(prefetch_files)
            }
            None => bail!("missing parameter `prefetch-files`"),
        }
    }

    fn get_bootstrap_storage(matches: &ArgMatches) -> Result<ArtifactStorage> {
        if let Some(s) = matches.get_one::<String>("bootstrap") {
            Ok(ArtifactStorage::SingleFile(s.into()))
        } else if let Some(d) = matches.get_one::<String>("blob-dir").map(PathBuf::from) {
            if !d.exists() {
                bail!("Directory to store blobs does not exist")
            }
            Ok(ArtifactStorage::FileDir((d, String::new())))
        } else {
            bail!("both --bootstrap and --blob-dir are missing, please specify one to store the generated metadata blob file");
        }
    }

    fn get_blob_cache_storage(
        matches: &ArgMatches,
        conversion_type: ConversionType,
    ) -> Result<Option<ArtifactStorage>> {
        if let Some(p) = matches.get_one::<PathBuf>("blob-cache-dir") {
            if conversion_type == ConversionType::TarToTarfs
                || conversion_type == ConversionType::EStargzIndexToRef
                || conversion_type == ConversionType::EStargzToRafs
                || conversion_type == ConversionType::EStargzToRef
            {
                bail!(
                    "conversion type `{}` conflicts with `--blob-cache-dir`",
                    conversion_type
                );
            }

            if !p.exists() {
                bail!("directory to store blob cache does not exist")
            }
            Ok(Some(ArtifactStorage::FileDir((
                p.to_owned(),
                String::new(),
            ))))
        } else {
            Ok(None)
        }
    }

    // Must specify a path to blob file.
    // For cli/binary interface compatibility sake, keep option `backend-config`, but
    // it only receives "localfs" backend type and it will be REMOVED in the future
    fn get_blob_storage(
        matches: &ArgMatches,
        conversion_type: ConversionType,
    ) -> Result<Option<ArtifactStorage>> {
        // Must specify a path to blob file.
        // For cli/binary interface compatibility sake, keep option `backend-config`, but
        // it only receives "localfs" backend type and it will be REMOVED in the future
        if conversion_type == ConversionType::EStargzIndexToRef {
            Ok(None)
        } else if let Some(p) = matches
            .get_one::<String>("blob")
            .map(|b| ArtifactStorage::SingleFile(b.into()))
        {
            if conversion_type == ConversionType::TarToTarfs {
                bail!(
                    "conversion type `{}` conflicts with `--blob`",
                    conversion_type
                );
            }
            Ok(Some(p))
        } else if let Some(d) = matches.get_one::<String>("blob-dir").map(PathBuf::from) {
            if !d.exists() {
                bail!("directory to store blobs does not exist")
            }
            Ok(Some(ArtifactStorage::FileDir((d, String::new()))))
        } else if let Some(config_json) = matches.get_one::<String>("backend-config") {
            let config: serde_json::Value = serde_json::from_str(config_json).unwrap();
            warn!("using --backend-type=localfs is DEPRECATED. Use --blob-dir instead.");
            if let Some(bf) = config.get("blob_file") {
                // Even unwrap, it is caused by invalid json. Image creation just can't start.
                let b: PathBuf = bf
                    .as_str()
                    .ok_or_else(|| anyhow!("backend-config is invalid"))?
                    .to_string()
                    .into();
                Ok(Some(ArtifactStorage::SingleFile(b)))
            } else {
                error!("Wrong backend config input!");
                Err(anyhow!("invalid backend config"))
            }
        } else {
            bail!("both --blob and --blob-dir are missing, please specify one to store the generated data blob file");
        }
    }

    fn get_parent_bootstrap(matches: &ArgMatches) -> Result<Option<String>> {
        let mut parent_bootstrap_path = String::new();
        if let Some(_parent_bootstrap_path) = matches.get_one::<String>("parent-bootstrap") {
            parent_bootstrap_path = _parent_bootstrap_path.to_string();
        }

        if !parent_bootstrap_path.is_empty() {
            Ok(Some(parent_bootstrap_path))
        } else {
            Ok(None)
        }
    }

    fn get_configuration(matches: &ArgMatches) -> Result<Arc<ConfigV2>> {
        let config = if let Some(config_file) = matches.get_one::<String>("config") {
            ConfigV2::from_file(config_file)?
        } else if let Some(dir) = matches.get_one::<String>("blob-dir") {
            ConfigV2::new_localfs("", dir)?
        } else {
            ConfigV2::default()
        };
        if !config.validate() {
            return Err(anyhow!("invalid configuration: {:?}", config));
        }

        Ok(Arc::new(config))
    }

    fn get_backend(
        matches: &ArgMatches,
        blob_id: &str,
    ) -> Result<(Arc<ConfigV2>, Arc<dyn BlobBackend + Send + Sync>)> {
        let config: Arc<ConfigV2>;
        let backend: Arc<dyn BlobBackend + Send + Sync>;
        if let Some(backend_type) = matches.get_one::<String>("backend-type") {
            let content =
                if let Some(backend_file) = matches.get_one::<String>("backend-config-file") {
                    fs::read_to_string(backend_file).with_context(|| {
                        format!("fail to read backend config file {:?}", backend_file)
                    })?
                } else if let Some(backend_config) = matches.get_one::<String>("backend-config") {
                    backend_config.clone()
                } else {
                    bail!("--backend-config or --backend-config-file must be specified");
                };

            if backend_type == "localfs" {
                bail!("Use --blob-dir to specify localfs backend");
            } else {
                backend = BlobFactory::new_backend_from_json(backend_type, &content, blob_id)?;
                config = Arc::new(ConfigV2::default());
            }
        } else if let Some(dir) = matches.get_one::<String>("blob-dir") {
            config = Arc::new(ConfigV2::new_localfs("", dir)?);
            backend = BlobFactory::new_backend(config.backend.as_ref().unwrap(), blob_id)?;
        } else {
            return Err(anyhow!("invalid backend configuration"));
        }

        Ok((config, backend))
    }

    fn get_blob_id(matches: &ArgMatches) -> Result<String> {
        let mut blob_id = String::new();

        if let Some(p_blob_id) = matches.get_one::<String>("blob-id") {
            blob_id = String::from(p_blob_id);
            if blob_id.len() > BLOB_ID_MAXIMUM_LENGTH {
                bail!("blob id is limited to length {}", BLOB_ID_MAXIMUM_LENGTH);
            }
        }

        Ok(blob_id)
    }

    fn get_blob_size(matches: &ArgMatches, ty: ConversionType) -> Result<u64> {
        if ty != ConversionType::EStargzIndexToRef {
            return Ok(0);
        }

        match matches.get_one::<String>("blob-data-size") {
            None => bail!("no value specified for '--blob-data-size'"),
            Some(v) => {
                let param = v.trim_start_matches("0x").trim_start_matches("0X");
                let size = u64::from_str_radix(param, 16)
                    .context(format!("invalid blob data size {}", v))?;
                Ok(size)
            }
        }
    }

    fn get_chunk_size(matches: &ArgMatches, ty: ConversionType) -> Result<u32> {
        match matches.get_one::<String>("chunk-size") {
            None => {
                if ty == ConversionType::EStargzIndexToRef {
                    Ok(0x400000u32)
                } else {
                    Ok(RAFS_DEFAULT_CHUNK_SIZE as u32)
                }
            }
            Some(v) => {
                let chunk_size = if v.starts_with("0x") || v.starts_with("0X") {
                    u32::from_str_radix(&v[2..], 16).context(format!("invalid chunk size {}", v))?
                } else {
                    v.parse::<u32>()
                        .context(format!("invalid chunk size {}", v))?
                };
                if chunk_size as u64 > RAFS_MAX_CHUNK_SIZE
                    || chunk_size < 0x1000
                    || !chunk_size.is_power_of_two()
                {
                    bail!("invalid chunk size: {}", chunk_size);
                }
                Ok(chunk_size)
            }
        }
    }

    fn get_batch_size(
        matches: &ArgMatches,
        version: RafsVersion,
        ty: ConversionType,
        chunk_size: u32,
    ) -> Result<u32> {
        match matches.get_one::<String>("batch-size") {
            None => Ok(0),
            Some(v) => {
                let batch_size = if v.starts_with("0x") || v.starts_with("0X") {
                    u32::from_str_radix(&v[2..], 16).context(format!("invalid batch size {}", v))?
                } else {
                    v.parse::<u32>()
                        .context(format!("invalid batch size {}", v))?
                };
                if batch_size > 0 {
                    if version.is_v5() {
                        bail!("`--batch-size` with non-zero value conflicts with `--fs-version 5`");
                    }
                    match ty {
                        ConversionType::DirectoryToRafs
                        | ConversionType::EStargzToRafs
                        | ConversionType::TargzToRafs
                        | ConversionType::TarToRafs => {
                            if batch_size as u64 > RAFS_MAX_CHUNK_SIZE
                                || batch_size < 0x1000
                                || !batch_size.is_power_of_two()
                            {
                                bail!("invalid batch size: {}", batch_size);
                            }
                            if batch_size > chunk_size {
                                bail!(
                                    "batch size 0x{:x} is bigger than chunk size 0x{:x}",
                                    batch_size,
                                    chunk_size
                                );
                            }
                        }
                        _ => bail!("unsupported ConversionType for batch chunk: {}", ty),
                    }
                }
                Ok(batch_size)
            }
        }
    }

    fn get_prefetch(matches: &ArgMatches) -> Result<Prefetch> {
        let prefetch_policy = matches
            .get_one::<String>("prefetch-policy")
            .map(|s| s.as_str())
            .unwrap_or_default()
            .parse()?;
        Prefetch::new(prefetch_policy)
    }

    fn get_blob_offset(matches: &ArgMatches) -> Result<u64> {
        match matches.get_one::<String>("blob-offset") {
            None => Ok(0),
            Some(v) => v
                .parse::<u64>()
                .context(format!("invalid blob offset {}", v)),
        }
    }

    fn get_fs_version(matches: &ArgMatches) -> Result<RafsVersion> {
        match matches.get_one::<String>("fs-version") {
            None => Ok(RafsVersion::V6),
            Some(v) => {
                let version: u32 = v.parse().context(format!("invalid fs-version: {}", v))?;
                if version == 5 {
                    Ok(RafsVersion::V5)
                } else if version == 6 {
                    Ok(RafsVersion::V6)
                } else {
                    bail!("invalid fs-version: {}", v);
                }
            }
        }
    }

    fn ensure_file<P: AsRef<Path>>(path: P) -> Result<()> {
        let file_type = metadata(path.as_ref())
            .context(format!("failed to access path {:?}", path.as_ref()))?
            .file_type();
        // The SOURCE can be a regular file, FIFO file, or /dev/stdin char device, etc..
        ensure!(
            file_type.is_file() || file_type.is_fifo() || file_type.is_char_device(),
            "specified path must be a regular/fifo/char_device file: {:?}",
            path.as_ref()
        );
        Ok(())
    }

    fn ensure_directory<P: AsRef<Path>>(path: P) -> Result<()> {
        let dir = metadata(path.as_ref())
            .context(format!("failed to access path {:?}", path.as_ref()))?;
        ensure!(
            dir.is_dir(),
            "specified path must be a directory: {:?}",
            path.as_ref()
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Command {
    fn export(args: &ArgMatches, subargs: &ArgMatches, build_info: &BuildTimeInfo) -> Result<()> {
        let subargs = nydus::SubCmdArgs::new(args, subargs);
        if subargs.is_present("block") {
            Self::export_block(&subargs, build_info)?;
        } else {
            bail!("unknown export type");
        }
        Ok(())
    }

    fn export_block(subargs: &nydus::SubCmdArgs, _bti: &BuildTimeInfo) -> Result<()> {
        let mut localfs_dir = None;
        let mut entry = if let Some(dir) = subargs.value_of("localfs-dir") {
            // Safe to unwrap because `--block` requires `--bootstrap`.
            let bootstrap = subargs.value_of("bootstrap").unwrap();
            let config = format!(
                r#"
            {{
                "type": "bootstrap",
                "id": "disk-default",
                "domain_id": "block-nbd",
                "config_v2": {{
                    "version": 2,
                    "id": "block-nbd-factory",
                    "backend": {{
                        "type": "localfs",
                        "localfs": {{
                            "dir": "{}"
                        }}
                    }},
                    "cache": {{
                        "type": "filecache",
                        "filecache": {{
                            "work_dir": "{}"
                        }}
                    }},
                    "metadata_path": "{}"
                }}
            }}"#,
                dir, dir, bootstrap
            );
            localfs_dir = Some(dir.to_string());
            nydus_api::BlobCacheEntry::from_str(&config)?
        } else if let Some(v) = subargs.value_of("config") {
            nydus_api::BlobCacheEntry::from_file(v)?
        } else {
            bail!("both option `-C/--config` and `-D/--localfs-dir` are missing");
        };
        if !entry.prepare_configuration_info() {
            bail!("invalid blob cache entry configuration information");
        }
        if !entry.validate() {
            bail!("invalid blob cache entry configuration information");
        }

        let threads: u32 = subargs
            .value_of("threads")
            .map(|n| n.parse().unwrap_or(1))
            .unwrap_or(1);
        let output = subargs.value_of("output").map(|v| v.to_string());
        let verity = subargs.is_present("verity");

        nydus_service::block_device::BlockDevice::export(
            entry,
            output,
            localfs_dir,
            threads,
            verity,
        )
        .context("failed to export RAFS filesystem as raw block device image")
    }

    fn thread_validator(v: &str) -> std::result::Result<String, String> {
        nydus_service::validate_threads_configuration(v).map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::Command;
    #[test]
    fn test_ensure_file() {
        Command::ensure_file("/dev/stdin").unwrap();
    }
}
