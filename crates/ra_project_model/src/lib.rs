//! FIXME: write short doc here

mod cargo_workspace;
mod json_project;
mod sysroot;

use std::{
    error::Error,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use ra_cfg::CfgOptions;
use ra_db::{CrateGraph, CrateId, CrateName, Edition, Env, FileId};
use rustc_hash::FxHashMap;
use serde_json::from_reader;

pub use crate::{
    cargo_workspace::{CargoFeatures, CargoWorkspace, Package, Target, TargetKind},
    json_project::JsonProject,
    sysroot::Sysroot,
};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CargoTomlNotFoundError(pub PathBuf);

impl std::fmt::Display for CargoTomlNotFoundError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "can't find Cargo.toml at {}", self.0.display())
    }
}

impl Error for CargoTomlNotFoundError {}

#[derive(Debug, Clone)]
pub enum ProjectWorkspace {
    /// Project workspace was discovered by running `cargo metadata` and `rustc --print sysroot`.
    Cargo { cargo: CargoWorkspace, sysroot: Sysroot },
    /// Project workspace was manually specified using a `rust-project.json` file.
    Json { project: JsonProject },
}

/// `PackageRoot` describes a package root folder.
/// Which may be an external dependency, or a member of
/// the current workspace.
#[derive(Clone)]
pub struct PackageRoot {
    /// Path to the root folder
    path: PathBuf,
    /// Is a member of the current workspace
    is_member: bool,
}

impl PackageRoot {
    pub fn new(path: PathBuf, is_member: bool) -> PackageRoot {
        PackageRoot { path, is_member }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn is_member(&self) -> bool {
        self.is_member
    }
}

impl ProjectWorkspace {
    pub fn discover(path: &Path, cargo_features: &CargoFeatures) -> Result<ProjectWorkspace> {
        ProjectWorkspace::discover_with_sysroot(path, true, cargo_features)
    }

    pub fn discover_with_sysroot(
        path: &Path,
        with_sysroot: bool,
        cargo_features: &CargoFeatures,
    ) -> Result<ProjectWorkspace> {
        match find_rust_project_json(path) {
            Some(json_path) => {
                let file = File::open(&json_path)
                    .with_context(|| format!("Failed to open json file {}", json_path.display()))?;
                let reader = BufReader::new(file);
                Ok(ProjectWorkspace::Json {
                    project: from_reader(reader).with_context(|| {
                        format!("Failed to deserialize json file {}", json_path.display())
                    })?,
                })
            }
            None => {
                let cargo_toml = find_cargo_toml(path).with_context(|| {
                    format!("Failed to find Cargo.toml for path {}", path.display())
                })?;
                let cargo = CargoWorkspace::from_cargo_metadata(&cargo_toml, cargo_features)
                    .with_context(|| {
                        format!(
                            "Failed to read Cargo metadata from Cargo.toml file {}",
                            cargo_toml.display()
                        )
                    })?;
                let sysroot = if with_sysroot {
                    Sysroot::discover(&cargo_toml).with_context(|| {
                        format!(
                            "Failed to find sysroot for Cargo.toml file {}",
                            cargo_toml.display()
                        )
                    })?
                } else {
                    Sysroot::default()
                };
                Ok(ProjectWorkspace::Cargo { cargo, sysroot })
            }
        }
    }

    /// Returns the roots for the current `ProjectWorkspace`
    /// The return type contains the path and whether or not
    /// the root is a member of the current workspace
    pub fn to_roots(&self) -> Vec<PackageRoot> {
        match self {
            ProjectWorkspace::Json { project } => {
                let mut roots = Vec::with_capacity(project.roots.len());
                for root in &project.roots {
                    roots.push(PackageRoot::new(root.path.clone(), true));
                }
                roots
            }
            ProjectWorkspace::Cargo { cargo, sysroot } => {
                let mut roots = Vec::with_capacity(cargo.packages().len() + sysroot.crates().len());
                for pkg in cargo.packages() {
                    let root = pkg.root(&cargo).to_path_buf();
                    let member = pkg.is_member(&cargo);
                    roots.push(PackageRoot::new(root, member));
                }
                for krate in sysroot.crates() {
                    roots.push(PackageRoot::new(krate.root_dir(&sysroot).to_path_buf(), false))
                }
                roots
            }
        }
    }

    pub fn n_packages(&self) -> usize {
        match self {
            ProjectWorkspace::Json { project } => project.crates.len(),
            ProjectWorkspace::Cargo { cargo, sysroot } => {
                cargo.packages().len() + sysroot.crates().len()
            }
        }
    }

    pub fn to_crate_graph(
        &self,
        default_cfg_options: &CfgOptions,
        load: &mut dyn FnMut(&Path) -> Option<FileId>,
    ) -> (CrateGraph, FxHashMap<CrateId, String>) {
        let mut crate_graph = CrateGraph::default();
        let mut names = FxHashMap::default();
        match self {
            ProjectWorkspace::Json { project } => {
                let mut crates = FxHashMap::default();
                for (id, krate) in project.crates.iter().enumerate() {
                    let crate_id = json_project::CrateId(id);
                    if let Some(file_id) = load(&krate.root_module) {
                        let edition = match krate.edition {
                            json_project::Edition::Edition2015 => Edition::Edition2015,
                            json_project::Edition::Edition2018 => Edition::Edition2018,
                        };
                        let cfg_options = {
                            let mut opts = default_cfg_options.clone();
                            for name in &krate.atom_cfgs {
                                opts.insert_atom(name.into());
                            }
                            for (key, value) in &krate.key_value_cfgs {
                                opts.insert_key_value(key.into(), value.into());
                            }
                            opts
                        };
                        crates.insert(
                            crate_id,
                            crate_graph.add_crate_root(
                                file_id,
                                edition,
                                cfg_options,
                                Env::default(),
                            ),
                        );
                    }
                }

                for (id, krate) in project.crates.iter().enumerate() {
                    for dep in &krate.deps {
                        let from_crate_id = json_project::CrateId(id);
                        let to_crate_id = dep.krate;
                        if let (Some(&from), Some(&to)) =
                            (crates.get(&from_crate_id), crates.get(&to_crate_id))
                        {
                            if crate_graph
                                .add_dep(from, CrateName::new(&dep.name).unwrap(), to)
                                .is_err()
                            {
                                log::error!(
                                    "cyclic dependency {:?} -> {:?}",
                                    from_crate_id,
                                    to_crate_id
                                );
                            }
                        }
                    }
                }
            }
            ProjectWorkspace::Cargo { cargo, sysroot } => {
                let mut sysroot_crates = FxHashMap::default();
                for krate in sysroot.crates() {
                    if let Some(file_id) = load(krate.root(&sysroot)) {
                        // Crates from sysroot have `cfg(test)` disabled
                        let cfg_options = {
                            let mut opts = default_cfg_options.clone();
                            opts.remove_atom("test");
                            opts
                        };

                        let crate_id = crate_graph.add_crate_root(
                            file_id,
                            Edition::Edition2018,
                            cfg_options,
                            Env::default(),
                        );
                        sysroot_crates.insert(krate, crate_id);
                        names.insert(crate_id, krate.name(&sysroot).to_string());
                    }
                }
                for from in sysroot.crates() {
                    for to in from.deps(&sysroot) {
                        let name = to.name(&sysroot);
                        if let (Some(&from), Some(&to)) =
                            (sysroot_crates.get(&from), sysroot_crates.get(&to))
                        {
                            if crate_graph.add_dep(from, CrateName::new(name).unwrap(), to).is_err()
                            {
                                log::error!("cyclic dependency between sysroot crates")
                            }
                        }
                    }
                }

                let libcore = sysroot.core().and_then(|it| sysroot_crates.get(&it).copied());
                let liballoc = sysroot.alloc().and_then(|it| sysroot_crates.get(&it).copied());
                let libstd = sysroot.std().and_then(|it| sysroot_crates.get(&it).copied());
                let libproc_macro =
                    sysroot.proc_macro().and_then(|it| sysroot_crates.get(&it).copied());

                let mut pkg_to_lib_crate = FxHashMap::default();
                let mut pkg_crates = FxHashMap::default();
                // Next, create crates for each package, target pair
                for pkg in cargo.packages() {
                    let mut lib_tgt = None;
                    for tgt in pkg.targets(&cargo) {
                        let root = tgt.root(&cargo);
                        if let Some(file_id) = load(root) {
                            let edition = pkg.edition(&cargo);
                            let cfg_options = {
                                let mut opts = default_cfg_options.clone();
                                opts.insert_features(pkg.features(&cargo).iter().map(Into::into));
                                opts
                            };
                            let crate_id = crate_graph.add_crate_root(
                                file_id,
                                edition,
                                cfg_options,
                                Env::default(),
                            );
                            names.insert(crate_id, pkg.name(&cargo).to_string());
                            if tgt.kind(&cargo) == TargetKind::Lib {
                                lib_tgt = Some(crate_id);
                                pkg_to_lib_crate.insert(pkg, crate_id);
                            }
                            if tgt.is_proc_macro(&cargo) {
                                if let Some(proc_macro) = libproc_macro {
                                    if crate_graph
                                        .add_dep(
                                            crate_id,
                                            CrateName::new("proc_macro").unwrap(),
                                            proc_macro,
                                        )
                                        .is_err()
                                    {
                                        log::error!(
                                            "cyclic dependency on proc_macro for {}",
                                            pkg.name(&cargo)
                                        )
                                    }
                                }
                            }

                            pkg_crates.entry(pkg).or_insert_with(Vec::new).push(crate_id);
                        }
                    }

                    // Set deps to the core, std and to the lib target of the current package
                    for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                        if let Some(to) = lib_tgt {
                            if to != from
                                && crate_graph
                                    .add_dep(
                                        from,
                                        // For root projects with dashes in their name,
                                        // cargo metadata does not do any normalization,
                                        // so we do it ourselves currently
                                        CrateName::normalize_dashes(pkg.name(&cargo)),
                                        to,
                                    )
                                    .is_err()
                            {
                                {
                                    log::error!(
                                        "cyclic dependency between targets of {}",
                                        pkg.name(&cargo)
                                    )
                                }
                            }
                        }
                        // core is added as a dependency before std in order to
                        // mimic rustcs dependency order
                        if let Some(core) = libcore {
                            if crate_graph
                                .add_dep(from, CrateName::new("core").unwrap(), core)
                                .is_err()
                            {
                                log::error!("cyclic dependency on core for {}", pkg.name(&cargo))
                            }
                        }
                        if let Some(alloc) = liballoc {
                            if crate_graph
                                .add_dep(from, CrateName::new("alloc").unwrap(), alloc)
                                .is_err()
                            {
                                log::error!("cyclic dependency on alloc for {}", pkg.name(&cargo))
                            }
                        }
                        if let Some(std) = libstd {
                            if crate_graph
                                .add_dep(from, CrateName::new("std").unwrap(), std)
                                .is_err()
                            {
                                log::error!("cyclic dependency on std for {}", pkg.name(&cargo))
                            }
                        }
                    }
                }

                // Now add a dep edge from all targets of upstream to the lib
                // target of downstream.
                for pkg in cargo.packages() {
                    for dep in pkg.dependencies(&cargo) {
                        if let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) {
                            for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                                if crate_graph
                                    .add_dep(from, CrateName::new(&dep.name).unwrap(), to)
                                    .is_err()
                                {
                                    log::error!(
                                        "cyclic dependency {} -> {}",
                                        pkg.name(&cargo),
                                        dep.pkg.name(&cargo)
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
        (crate_graph, names)
    }

    pub fn workspace_root_for(&self, path: &Path) -> Option<&Path> {
        match self {
            ProjectWorkspace::Cargo { cargo, .. } => {
                Some(cargo.workspace_root()).filter(|root| path.starts_with(root))
            }
            ProjectWorkspace::Json { project: JsonProject { roots, .. } } => roots
                .iter()
                .find(|root| path.starts_with(&root.path))
                .map(|root| root.path.as_ref()),
        }
    }
}

fn find_rust_project_json(path: &Path) -> Option<PathBuf> {
    if path.ends_with("rust-project.json") {
        return Some(path.to_path_buf());
    }

    let mut curr = Some(path);
    while let Some(path) = curr {
        let candidate = path.join("rust-project.json");
        if candidate.exists() {
            return Some(candidate);
        }
        curr = path.parent();
    }

    None
}

fn find_cargo_toml(path: &Path) -> Result<PathBuf> {
    if path.ends_with("Cargo.toml") {
        return Ok(path.to_path_buf());
    }
    let mut curr = Some(path);
    while let Some(path) = curr {
        let candidate = path.join("Cargo.toml");
        if candidate.exists() {
            return Ok(candidate);
        }
        curr = path.parent();
    }
    Err(CargoTomlNotFoundError(path.to_path_buf()).into())
}

pub fn get_rustc_cfg_options() -> CfgOptions {
    let mut cfg_options = CfgOptions::default();

    // Some nightly-only cfgs, which are required for stdlib
    {
        cfg_options.insert_atom("target_thread_local".into());
        for &target_has_atomic in ["8", "16", "32", "64", "cas", "ptr"].iter() {
            cfg_options.insert_key_value("target_has_atomic".into(), target_has_atomic.into());
            cfg_options
                .insert_key_value("target_has_atomic_load_store".into(), target_has_atomic.into());
        }
    }

    match (|| -> Result<String> {
        // `cfg(test)` and `cfg(debug_assertion)` are handled outside, so we suppress them here.
        let output = Command::new("rustc")
            .args(&["--print", "cfg", "-O"])
            .output()
            .context("Failed to get output from rustc --print cfg -O")?;
        if !output.status.success() {
            bail!(
                "rustc --print cfg -O exited with exit code ({})",
                output
                    .status
                    .code()
                    .map_or(String::from("no exit code"), |code| format!("{}", code))
            );
        }
        Ok(String::from_utf8(output.stdout)?)
    })() {
        Ok(rustc_cfgs) => {
            for line in rustc_cfgs.lines() {
                match line.find('=') {
                    None => cfg_options.insert_atom(line.into()),
                    Some(pos) => {
                        let key = &line[..pos];
                        let value = line[pos + 1..].trim_matches('"');
                        cfg_options.insert_key_value(key.into(), value.into());
                    }
                }
            }
        }
        Err(e) => log::error!("failed to get rustc cfgs: {}", e),
    }

    cfg_options
}
