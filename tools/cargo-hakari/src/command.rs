// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{
    helpers::{read_contents, regenerate_lockfile},
    output::{OutputContext, OutputOpts},
    publish::publish_hakari,
};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{AppSettings, Parser};
use color_eyre::eyre::{bail, eyre, Result, WrapErr};
use guppy::{
    graph::{PackageGraph, PackageSet},
    MetadataCommand,
};
use hakari::{
    cli_ops::{HakariInit, WorkspaceOps},
    diffy::PatchFormatter,
    summaries::{HakariConfig, DEFAULT_CONFIG_PATH, FALLBACK_CONFIG_PATH},
    HakariBuilder, HakariCargoToml, HakariOutputOptions, TomlOutError,
};
use log::{error, info};
use owo_colors::OwoColorize;
use std::convert::TryFrom;

/// The comment to add to the top of the config file.
pub static CONFIG_COMMENT: &str = r#"# This file contains settings for `cargo hakari`.
# See https://docs.rs/cargo-hakari/latest/cargo_hakari/config for a full list of options.
"#;

/// The comment to add to the top of the workspace-hack package's Cargo.toml.
pub static CARGO_TOML_COMMENT: &str = r#"# This file is generated by `cargo hakari`.
# To regenerate, run:
#     cargo hakari generate
"#;

/// The message to write into a disabled Cargo.toml.
pub static DISABLE_MESSAGE: &str = r#"
# Disabled by running `cargo hakari disable`.
# To re-enable, run:
#     cargo hakari generate
"#;

/// Set up and manage workspace-hack crates.
///
/// For more about cargo-hakari, see <https://docs.rs/cargo-hakari>.
#[derive(Debug, Parser)]
#[clap(author, version, about)]
pub struct Args {
    #[clap(flatten)]
    global: GlobalOpts,
    #[clap(subcommand)]
    command: Command,
}

impl Args {
    /// Executes the command.
    ///
    /// Returns the exit status, or an error on failure.
    pub fn exec(self) -> Result<i32> {
        self.command.exec(self.global.output)
    }
}

#[derive(Debug, Parser)]
struct GlobalOpts {
    #[clap(flatten)]
    output: OutputOpts,
}

/// Manage workspace-hack crates.
#[derive(Debug, Parser)]
enum Command {
    /// Initialize a workspace-hack crate and a hakari.toml file
    #[clap(name = "init")]
    Initialize {
        /// Path to generate the workspace-hack crate at, relative to the current directory.
        path: Utf8PathBuf,

        /// The name of the crate (default: derived from path)
        #[clap(long, short)]
        package_name: Option<String>,

        /// Skip writing a stub config to hakari.toml
        #[clap(long)]
        skip_config: bool,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[clap(long, short = 'n', conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[clap(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    #[structopt(flatten)]
    WithBuilder(CommandWithBuilder),
}

impl Command {
    fn exec(self, output: OutputOpts) -> Result<i32> {
        let output = output.init();
        let metadata_command = MetadataCommand::new();
        let package_graph = metadata_command
            .build_graph()
            .context("building package graph failed")?;

        match self {
            Command::Initialize {
                path,
                package_name,
                skip_config,
                dry_run,
                yes,
            } => {
                let package_name = match package_name.as_deref() {
                    Some(name) => name,
                    None => match path.file_name() {
                        Some(name) => name,
                        None => bail!("invalid path {}", path),
                    },
                };

                let workspace_path =
                    cwd_rel_to_workspace_rel(&path, package_graph.workspace().root())?;

                let mut init = HakariInit::new(&package_graph, package_name, &workspace_path)
                    .with_context(|| "error initializing Hakari package")?;
                init.set_cargo_toml_comment(CARGO_TOML_COMMENT);
                if !skip_config {
                    init.set_config(DEFAULT_CONFIG_PATH.as_ref(), CONFIG_COMMENT)
                        .with_context(|| "error initializing Hakari package")?;
                }

                let ops = init.make_ops();
                apply_on_dialog(dry_run, yes, &ops, &output, || {
                    let steps = [
                        format!(
                            "* configure at {}",
                            DEFAULT_CONFIG_PATH.style(output.styles.config_path),
                        ),
                        format!(
                            "* run {} to generate contents",
                            "cargo hakari generate".style(output.styles.command),
                        ),
                        format!(
                            "* run {} to add dependency lines",
                            "cargo hakari manage-deps".style(output.styles.command),
                        ),
                    ];
                    info!("next steps:\n{}\n", steps.join("\n"));
                    Ok(())
                })
            }
            Command::WithBuilder(cmd) => {
                let (builder, hakari_output) = make_builder_and_output(&package_graph)?;
                cmd.exec(builder, hakari_output, output)
            }
        }
    }
}

#[derive(Debug, Parser)]
enum CommandWithBuilder {
    /// Generate or update the contents of the workspace-hack crate
    Generate {
        /// Print a diff of contents instead of writing them out. Can be combined with `--quiet`.
        ///
        /// Exits with status 1 if the contents are different.
        #[clap(long)]
        diff: bool,
    },

    /// Perform verification of the workspace-hack crate
    ///
    /// Check that the workspace-hack crate succeeds at its goal of building one version of
    /// every non-omitted third-party crate.
    ///
    /// Exits with status 1 if verification failed.
    Verify,

    /// Manage dependencies from workspace crates to workspace-hack.
    ///
    /// * Add the dependency to all non-excluded workspace crates.
    /// * Remove the dependency from all excluded workspace crates.
    ManageDeps {
        #[clap(flatten)]
        packages: PackageSelection,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[clap(long, short = 'n', conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[structopt(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    /// Remove dependencies from workspace crates to workspace-hack.
    RemoveDeps {
        #[clap(flatten)]
        packages: PackageSelection,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[structopt(long, short = 'n', conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[structopt(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    /// Print out workspace crates responsible for adding a dependency to workspace-hack.
    ///
    /// For a dependency to be included in the workspace-hack, it must have been built with at least
    /// two different feature sets by different crates in the workspace (unless the
    /// output-single-feature option is set to true). The explain command prints out a table
    /// consisting of the different feature sets that got built; and, for each feature set, the
    /// workspace crates and options that resulted in it.
    ///
    /// Adding the initial set of dependencies to the workspace-hack can cause further dependencies
    /// to be added if they're built with a second feature set. These cases are marked as
    /// "post-compute fixup".
    ///
    /// Currently, this command only prints out the different feature sets that get built for a
    /// dependency, and the workspace crates responsible for them. Further investigation can be done
    /// through `cargo tree`. In the future, the scope of this command may be extended to provide
    /// information about intermediate dependencies as well.
    Explain {
        /// The name of the dependency, as present in the workspace-hack.
        dep_name: String,
    },

    /// Publish a package after temporarily removing the workspace-hack dependency from it.
    ///
    /// For more information about publishing options,
    /// see {n}https://docs.rs/cargo-hakari/latest/cargo_hakari/publishing.
    ///
    /// Trailing arguments are passed through to cargo publish.
    #[clap(setting = AppSettings::TrailingVarArg, setting = AppSettings::AllowHyphenValues)]
    Publish {
        /// The name of the package to publish.
        #[structopt(long, short)]
        package: String,

        /// Arguments to pass through to `cargo publish`.
        #[structopt(multiple_values = true)]
        pass_through: Vec<String>,
    },

    /// Disables the workspace-hack crate.
    ///
    /// Removes all the generated contents from the workspace-hack crate.
    Disable {
        /// Print a diff of changes instead of writing them out. Can be combined with `--quiet`.
        ///
        /// Exits with status 1 if the contents are different.
        #[structopt(long)]
        diff: bool,
    },
}

impl CommandWithBuilder {
    fn exec(
        self,
        builder: HakariBuilder<'_>,
        hakari_output: HakariOutputOptions,
        output: OutputContext,
    ) -> Result<i32> {
        let hakari_package = *builder
            .hakari_package()
            .expect("hakari-package must be specified in hakari.toml");

        match self {
            CommandWithBuilder::Generate { diff } => {
                let package_graph = builder.graph();
                let hakari = builder.compute();
                let toml_out = match hakari.to_toml_string(&hakari_output) {
                    Ok(toml_out) => toml_out,
                    Err(TomlOutError::UnrecognizedRegistry {
                        package_id,
                        registry_url,
                    }) => {
                        // Print out a better error message for this more common use case.
                        let package = package_graph
                            .metadata(&package_id)
                            .expect("package ID obtained from the same graph");
                        error!(
                            "unrecognized registry URL {} found for {} v{}\n\
                             (add to [registries] section of {})",
                            registry_url.style(output.styles.registry_url),
                            package.name().style(output.styles.package_name),
                            package.version().style(output.styles.package_version),
                            "hakari.toml".style(output.styles.config_path),
                        );
                        // 102 is picked pretty arbitrarily because regular errors exit with 101.
                        return Ok(102);
                    }
                    Err(err) => Err(err).with_context(|| "error generating new hakari.toml")?,
                };

                let existing_toml = hakari
                    .read_toml()
                    .expect("hakari-package must be specified")?;

                write_to_cargo_toml(existing_toml, &toml_out, diff, output)
            }
            CommandWithBuilder::Verify => match builder.verify() {
                Ok(()) => {
                    info!(
                        "{} works correctly",
                        hakari_package.name().style(output.styles.package_name),
                    );
                    Ok(0)
                }
                Err(errs) => {
                    let mut display = errs.display();
                    if output.color.is_enabled() {
                        display.colorize();
                    }
                    info!(
                        "{} didn't work correctly:\n{}",
                        hakari_package.name().style(output.styles.package_name),
                        display,
                    );
                    Ok(1)
                }
            },
            CommandWithBuilder::ManageDeps {
                packages,
                dry_run,
                yes,
            } => {
                let ops = builder
                    .manage_dep_ops(&packages.to_package_set(builder.graph())?)
                    .expect("hakari-package must be specified in hakari.toml");
                if ops.is_empty() {
                    info!("no operations to perform");
                    return Ok(0);
                }

                apply_on_dialog(dry_run, yes, &ops, &output, || {
                    regenerate_lockfile(output.clone())
                })
            }
            CommandWithBuilder::RemoveDeps {
                packages,
                dry_run,
                yes,
            } => {
                let ops = builder
                    .remove_dep_ops(&packages.to_package_set(builder.graph())?, false)
                    .expect("hakari-package must be specified in hakari.toml");
                if ops.is_empty() {
                    info!("no operations to perform");
                    return Ok(0);
                }

                apply_on_dialog(dry_run, yes, &ops, &output, || {
                    regenerate_lockfile(output.clone())
                })
            }
            CommandWithBuilder::Explain {
                dep_name: crate_name,
            } => {
                let hakari = builder.compute();
                let toml_name_map = hakari.toml_name_map();
                let dep = toml_name_map.get(crate_name.as_str()).ok_or_else(|| {
                    eyre!(
                        "crate name '{}' not found in workspace-hack\n\
                        (hint: check spelling, or regenerate workspace-hack with `cargo hakari generate`)",
                        crate_name
                    )
                })?;

                let explain = hakari
                    .explain(dep.id())
                    .expect("package ID should be known since it was in the output");
                let mut display = explain.display();
                if output.color.is_enabled() {
                    display.colorize();
                }
                info!("\n{}", display);
                Ok(0)
            }
            CommandWithBuilder::Publish {
                package,
                pass_through,
            } => {
                publish_hakari(&package, builder, &pass_through, output)?;
                Ok(0)
            }
            CommandWithBuilder::Disable { diff } => {
                let existing_toml = builder
                    .read_toml()
                    .expect("hakari-package must be specified")?;
                write_to_cargo_toml(existing_toml, DISABLE_MESSAGE, diff, output)
            }
        }
    }
}

/// Support for packages and features.
#[derive(Debug, Parser)]
struct PackageSelection {
    #[clap(long = "package", short)]
    /// Packages to operate on (default: entire workspace)
    packages: Vec<String>,
}

impl PackageSelection {
    /// Converts this selection into a `PackageSet`.
    fn to_package_set<'g>(&self, graph: &'g PackageGraph) -> Result<PackageSet<'g>> {
        if !self.packages.is_empty() {
            Ok(graph.resolve_workspace_names(&self.packages)?)
        } else {
            Ok(graph.resolve_workspace())
        }
    }
}

// ---
// Helper methods
// ---

fn cwd_rel_to_workspace_rel(path: &Utf8Path, workspace_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let abs_path = if path.is_absolute() {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir().with_context(|| "could not access current dir")?;
        let mut cwd = Utf8PathBuf::try_from(cwd).with_context(|| "current dir is invalid UTF-8")?;
        cwd.push(path);
        cwd
    };

    abs_path
        .strip_prefix(workspace_root)
        .map(|p| p.to_owned())
        .with_context(|| {
            format!(
                "path {} is not inside workspace root {}",
                abs_path, workspace_root
            )
        })
}

fn make_builder_and_output(
    package_graph: &PackageGraph,
) -> Result<(HakariBuilder<'_>, HakariOutputOptions)> {
    let (config_path, contents) = read_contents(
        package_graph.workspace().root(),
        [DEFAULT_CONFIG_PATH, FALLBACK_CONFIG_PATH],
    )
    .wrap_err("error reading Hakari config")?;

    let config: HakariConfig = contents
        .parse()
        .wrap_err_with(|| format!("error deserializing Hakari config at {}", config_path))?;

    let builder = config
        .builder
        .to_hakari_builder(package_graph)
        .wrap_err_with(|| format!("error resolving Hakari config at {}", config_path))?;
    let hakari_output = config.output.to_options();

    Ok((builder, hakari_output))
}

fn write_to_cargo_toml(
    existing_toml: HakariCargoToml,
    new_contents: &str,
    diff: bool,
    output: OutputContext,
) -> Result<i32> {
    if diff {
        let patch = existing_toml.diff_toml(new_contents);
        let mut formatter = PatchFormatter::new();
        if output.color.is_enabled() {
            formatter = formatter.with_color();
        }
        info!("\n{}", formatter.fmt_patch(&patch));
        if patch.hunks().is_empty() {
            // No differences.
            Ok(0)
        } else {
            Ok(1)
        }
    } else {
        if !existing_toml.is_changed(new_contents) {
            info!("no changes detected");
        } else {
            existing_toml
                .write_to_file(new_contents)
                .with_context(|| "error writing updated Hakari contents")?;
            info!("contents updated");
            regenerate_lockfile(output)?;
        }
        Ok(0)
    }
}

fn apply_on_dialog(
    dry_run: bool,
    yes: bool,
    ops: &WorkspaceOps<'_, '_>,
    output: &OutputContext,
    after: impl FnOnce() -> Result<()>,
) -> Result<i32> {
    let mut display = ops.display();
    if output.color.is_enabled() {
        display.colorize();
    }
    info!("operations to perform:\n\n{}", display);

    if dry_run {
        // dry-run + non-empty ops implies exit status 1.
        return Ok(1);
    }

    let should_apply = if yes {
        true
    } else {
        let colorful_theme = dialoguer::theme::ColorfulTheme::default();
        let mut confirm = if output.color.is_enabled() {
            dialoguer::Confirm::with_theme(&colorful_theme)
        } else {
            dialoguer::Confirm::with_theme(&dialoguer::theme::SimpleTheme)
        };
        confirm
            .with_prompt("proceed?")
            .default(true)
            .show_default(true)
            .interact()
            .with_context(|| "error reading input")?
    };

    if should_apply {
        ops.apply()?;
        after()?;
        Ok(0)
    } else {
        Ok(1)
    }
}
