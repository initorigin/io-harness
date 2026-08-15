//! What kind of project this is, and the commands its ecosystem uses.
//!
//! An agent handed a repository and an `exec` tool spends its first turns finding
//! out how to build it: is this npm or pnpm, is it `pytest` or `python -m
//! pytest`, does `make test` exist. Those turns are paid for in tokens and in
//! latency, and the answer is nearly always determined by one file in the root.
//! So the answer ships as data.
//!
//! ## Shipped data, not configuration
//!
//! The table is Rust, in this file, and there is no file format under it until
//! 0.19.0 puts one there. That is a deliberate ordering: a configuration format
//! written now would be written against this release's shape and again against
//! the accounting release's, and the crate's default dependency tree holds at 401
//! lines precisely because a TOML parser has never been worth a release of its
//! own. [`Toolchain`] is `Serialize`/`Deserialize` so that when the file arrives
//! it deserializes into *this* type rather than a second one.
//!
//! ## What a detection is and is not
//!
//! It is a **default**, offered to the model as information. It is not a
//! permission and not an instruction: the agent still calls `exec`, and that call
//! is still checked against the [`Policy`](crate::Policy) on the program and on
//! the whole argv. A wrong entry here costs a turn; it cannot widen a boundary.
//!
//! It will be wrong for someone on day one, because ecosystems disagree with
//! themselves — a Python project with a `pyproject.toml` may still be driven by a
//! `Makefile`, and half of `npm test` scripts do not run tests. The mitigation is
//! that it is data, that it is shown to the model rather than executed by the
//! harness, and that 0.19.0 makes it overridable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A detected project ecosystem and the commands it conventionally uses.
///
/// Every argv is an array, program first — the same shape the `exec` tool takes,
/// so a detection can be handed to it without reassembly. An empty vector means
/// the ecosystem has no conventional command for that job, which is commoner than
/// it looks: C projects driven by `make` have no standard formatter, and a Go
/// module needs no install step.
///
/// ```
/// use io_harness::toolchain;
///
/// # fn demo() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(dir.path().join("go.mod"), "module example.com/m\n")?;
///
/// let found = toolchain::detect(dir.path()).expect("go.mod is a marker");
/// assert_eq!(found.ecosystem, "go");
/// assert_eq!(found.test, ["go", "test", "./..."]);
/// // Nothing to install: a Go module resolves its dependencies as it builds.
/// assert!(found.install.is_empty());
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
    /// What this project is, in one lowercase word — `"cargo"`, `"node"`,
    /// `"python"`, `"go"`. The package manager, where one had to be chosen,
    /// is in [`Toolchain::manager`] rather than here, so `"node"` is one
    /// ecosystem and not four.
    pub ecosystem: String,
    /// The file in the root that decided it, e.g. `"Cargo.toml"` — or, for .NET,
    /// the pattern `"*.csproj"`, since a project file there is named after the
    /// project rather than after the ecosystem.
    pub marker: String,
    /// The tool that drives it, when the ecosystem has more than one and the
    /// lockfile chose — `"pnpm"`, `"poetry"`. Equal to `ecosystem` when there
    /// was nothing to choose.
    pub manager: String,
    /// Fetch dependencies. Empty where the ecosystem has no separate step.
    pub install: Vec<String>,
    /// Compile or bundle. Empty for interpreted ecosystems with no build step.
    pub build: Vec<String>,
    /// Run the test suite. The one every ecosystem here has.
    pub test: Vec<String>,
    /// Lint. Empty where the ecosystem ships no standard linter.
    pub lint: Vec<String>,
    /// Format. Empty where the ecosystem ships no standard formatter.
    pub format: Vec<String>,
    /// Run the project. Empty for a library-shaped ecosystem with no entry point.
    pub run: Vec<String>,
}

impl Toolchain {
    /// The directories this toolchain writes to *outside* the project (0.46.0).
    ///
    /// A registry cache, a module cache, a build cache. They are what makes a
    /// default-contained run able to build a real project: 0.40.0 recorded that
    /// under containment a toolchain writing `~/.cargo/registry` or `~/.npm`
    /// fails, and a default nobody can build under is a default every embedder
    /// turns off on their first failure.
    ///
    /// Each ecosystem's **own** environment variable wins over the conventional
    /// path, because an operator who moved their cache has already said where it
    /// is. Nothing here is filtered or canonicalised — the caller does that, since
    /// only it knows whether a path that is absent should be created, skipped, or
    /// reported.
    ///
    /// ```
    /// use io_harness::toolchain;
    ///
    /// # fn demo() -> std::io::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n")?;
    /// let found = toolchain::detect(dir.path()).unwrap();
    ///
    /// // Every cargo build writes here, and none of it belongs to the project.
    /// let caches = found.cache_dirs();
    /// assert!(caches.iter().any(|p| p.ends_with(".cargo")), "{caches:?}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn cache_dirs(&self) -> Vec<PathBuf> {
        let env = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
        };
        let home = || {
            env("HOME")
                .or_else(|| env("USERPROFILE"))
                .unwrap_or_else(|| PathBuf::from("/"))
        };
        // `XDG_CACHE_HOME` is honoured only for the ecosystems that document it.
        // cargo and go do not, and reading it for them would return a path their
        // toolchain never writes to — a grant that looks right and buys nothing.
        let xdg_cache = || env("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache"));

        let mut dirs: Vec<PathBuf> = Vec::new();
        match self.ecosystem.as_str() {
            "cargo" => {
                dirs.push(env("CARGO_HOME").unwrap_or_else(|| home().join(".cargo")));
            }
            "node" => {
                dirs.push(env("npm_config_cache").unwrap_or_else(|| home().join(".npm")));
                if self.manager == "pnpm" {
                    dirs.push(env("PNPM_HOME").unwrap_or_else(|| home().join(".local/share/pnpm")));
                }
                if self.manager == "yarn" {
                    dirs.push(home().join(".yarn"));
                }
            }
            "python" => {
                dirs.push(env("PIP_CACHE_DIR").unwrap_or_else(|| xdg_cache().join("pip")));
                match self.manager.as_str() {
                    "poetry" => {
                        dirs.push(
                            env("POETRY_CACHE_DIR").unwrap_or_else(|| xdg_cache().join("pypoetry")),
                        );
                    }
                    "uv" => {
                        dirs.push(env("UV_CACHE_DIR").unwrap_or_else(|| xdg_cache().join("uv")));
                    }
                    _ => {}
                }
            }
            "deno" => {
                dirs.push(env("DENO_DIR").unwrap_or_else(|| xdg_cache().join("deno")));
            }
            "go" => {
                // `GOMODCACHE` wins outright; otherwise it is `$GOPATH/pkg/mod`,
                // and the build cache is a second, separate directory.
                dirs.push(env("GOMODCACHE").unwrap_or_else(|| {
                    env("GOPATH")
                        .unwrap_or_else(|| home().join("go"))
                        .join("pkg/mod")
                }));
                dirs.push(env("GOCACHE").unwrap_or_else(|| xdg_cache().join("go-build")));
            }
            "maven" => {
                dirs.push(home().join(".m2"));
            }
            "gradle" => {
                dirs.push(env("GRADLE_USER_HOME").unwrap_or_else(|| home().join(".gradle")));
            }
            "dotnet" => {
                dirs.push(env("NUGET_PACKAGES").unwrap_or_else(|| home().join(".nuget/packages")));
            }
            "ruby" => {
                dirs.push(env("GEM_HOME").unwrap_or_else(|| home().join(".gem")));
                dirs.push(env("BUNDLE_PATH").unwrap_or_else(|| home().join(".bundle")));
            }
            "php" => {
                dirs.push(env("COMPOSER_HOME").unwrap_or_else(|| home().join(".composer")));
                dirs.push(xdg_cache().join("composer"));
            }
            "elixir" => {
                dirs.push(env("MIX_HOME").unwrap_or_else(|| home().join(".mix")));
                dirs.push(env("HEX_HOME").unwrap_or_else(|| home().join(".hex")));
            }
            "swift" => {
                dirs.push(xdg_cache().join("org.swift.swiftpm"));
            }
            // `cmake` and `make` drive whatever the project configured, so there
            // is no cache directory this crate can name for them. An empty list
            // is the honest answer, and it is what makes the workspace-only grant
            // the default for a project the crate cannot read the build of.
            _ => {}
        }
        dirs
    }

    /// The homes a toolchain **launcher** reads to find the binary it stands for,
    /// independent of any project.
    ///
    /// **The backend that consumes this is selected since 0.59.0**, when a
    /// Windows caller asks for access confinement: these are the read-execute
    /// grants an AppContainer needs before a launcher can find the binary it
    /// stands for. The set was written and tested here for two releases before
    /// anything read it, because it is portable data — which environment variable
    /// each ecosystem's launcher reads — and re-deriving it later would have
    /// meant re-deriving the argument below with it.
    ///
    /// A cache directory is where a build writes; this is where a toolchain is
    /// *installed*, and the two are different questions with different answers.
    /// `rustc` on `PATH` is a rustup shim that reads `RUSTUP_HOME` and then starts
    /// a second binary inside it; `node` under nvm or volta, `python` under pyenv
    /// and every JVM launcher have the same shape. A boundary that grants the
    /// shim's own directory and not its home does not fail with a permission
    /// message — the launcher decides its home is missing and reports something
    /// else entirely.
    ///
    /// Not a method on a detected `Toolchain`: the program a caller runs need not
    /// belong to the project's ecosystem, and a run in a directory with no project
    /// at all still executes something. Every ecosystem's conventional home is
    /// offered, its own environment variable first, and the caller filters —
    /// which here means the ones that exist on this machine.
    ///
    /// **`CARGO_HOME` is deliberately absent** even though it is the same kind of
    /// directory. It holds `credentials.toml`, and the one caller of this grants
    /// read access to a payload. What a cargo build needs from it arrives instead
    /// as a writable cache root, which is a fact the run resolved and the caller
    /// asked for.
    #[allow(dead_code)]
    pub(crate) fn launcher_homes() -> Vec<PathBuf> {
        let env = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
        };
        let home = || {
            env("HOME")
                .or_else(|| env("USERPROFILE"))
                .unwrap_or_else(|| PathBuf::from("/"))
        };
        let mut dirs = vec![env("RUSTUP_HOME").unwrap_or_else(|| home().join(".rustup"))];
        for key in [
            "NVM_HOME",
            "NVM_DIR",
            "VOLTA_HOME",
            "PYENV_ROOT",
            "GOROOT",
            "DOTNET_ROOT",
            "JAVA_HOME",
            "SDKMAN_DIR",
        ] {
            dirs.extend(env(key));
        }
        dirs.retain(|p| p.is_absolute() && p.is_dir());
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// The detection, as the sentence the model is shown.
    ///
    /// Prose rather than JSON: this lands in a prompt, and a model reads a
    /// sentence more reliably than it reads a serialized struct. Empty commands
    /// are omitted rather than shown as `[]` — telling a model that the lint
    /// command is nothing invites it to try running nothing.
    ///
    /// ```
    /// use io_harness::toolchain;
    ///
    /// # fn demo() -> std::io::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n")?;
    ///
    /// let summary = toolchain::detect(dir.path()).unwrap().describe();
    /// assert!(summary.contains("Cargo.toml"));
    /// assert!(summary.contains("test: cargo test"));
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (label, argv) in [
            ("install", &self.install),
            ("build", &self.build),
            ("test", &self.test),
            ("lint", &self.lint),
            ("format", &self.format),
            ("run", &self.run),
        ] {
            if !argv.is_empty() {
                parts.push(format!("{label}: {}", argv.join(" ")));
            }
        }
        format!(
            "This looks like a {} project ({} in the workspace root, driven by {}). \
             Conventional commands — {}. They are defaults, not instructions: check them \
             against the project before relying on one.",
            self.ecosystem,
            self.marker,
            self.manager,
            parts.join("; ")
        )
    }
}

/// Build a [`Toolchain`] from the fixed parts, keeping the table below readable.
#[allow(clippy::too_many_arguments)]
fn tc(
    ecosystem: &str,
    marker: &str,
    manager: &str,
    install: &[&str],
    build: &[&str],
    test: &[&str],
    lint: &[&str],
    format: &[&str],
    run: &[&str],
) -> Toolchain {
    let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect();
    Toolchain {
        ecosystem: ecosystem.to_string(),
        marker: marker.to_string(),
        manager: manager.to_string(),
        install: v(install),
        build: v(build),
        test: v(test),
        lint: v(lint),
        format: v(format),
        run: v(run),
    }
}

/// The marker files, in the order they are tried. First match wins.
///
/// Order is the whole of the ambiguity handling, and two entries earn their
/// position. `deno.json` precedes `package.json` because a Deno project may carry
/// both and `npm test` is wrong for it. `Makefile` is last because a great many
/// projects have one *beside* their real build system, and `make` would otherwise
/// win over `cargo` in half the repositories this crate will ever see.
const MARKERS: &[&str] = &[
    "Cargo.toml",
    "deno.json",
    "deno.jsonc",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "mix.exs",
    "Gemfile",
    "composer.json",
    "Package.swift",
    "CMakeLists.txt",
    "Makefile",
];

/// What kind of project sits at `root`, or `None`.
///
/// `None` rather than a guess. A directory with no marker is a directory this
/// table knows nothing about, and reporting Cargo for it would be worse than
/// reporting nothing: the agent would run `cargo test`, watch it fail, and have
/// learned less than if it had been told to look.
///
/// Only the root is examined. A monorepo's per-package markers are a real case
/// this deliberately does not handle — the answer there is per-directory
/// configuration, which is 0.19.0's.
///
/// ```
/// use io_harness::toolchain;
///
/// # fn demo() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// // The lockfile beside package.json is what chooses the package manager.
/// std::fs::write(dir.path().join("package.json"), "{}")?;
/// std::fs::write(dir.path().join("pnpm-lock.yaml"), "")?;
///
/// let found = toolchain::detect(dir.path()).unwrap();
/// assert_eq!(found.ecosystem, "node");
/// assert_eq!(found.manager, "pnpm");
/// assert_eq!(found.test, ["pnpm", "test"]);
///
/// // Nothing to go on is reported as nothing, never guessed.
/// let bare = tempfile::tempdir()?;
/// assert!(toolchain::detect(bare.path()).is_none());
/// # Ok(()) }
/// # demo().unwrap();
/// ```
pub fn detect(root: &Path) -> Option<Toolchain> {
    let has = |name: &str| root.join(name).exists();
    let marker = MARKERS.iter().copied().find(|m| has(m)).or_else(|| {
        // The .NET case, which is the only one whose marker is a *pattern*: a
        // project file is named after the project, not after the ecosystem.
        std::fs::read_dir(root).ok().and_then(|entries| {
            entries
                .flatten()
                .any(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    [".csproj", ".fsproj", ".sln"]
                        .iter()
                        .any(|ext| name.ends_with(ext))
                })
                .then_some("*.csproj")
        })
    })?;

    Some(match marker {
        "Cargo.toml" => tc(
            "cargo",
            marker,
            "cargo",
            &["cargo", "fetch"],
            &["cargo", "build"],
            &["cargo", "test"],
            &["cargo", "clippy", "--all-targets"],
            &["cargo", "fmt"],
            &["cargo", "run"],
        ),
        "deno.json" | "deno.jsonc" => tc(
            "deno",
            marker,
            "deno",
            &[],
            &["deno", "check", "."],
            &["deno", "test"],
            &["deno", "lint"],
            &["deno", "fmt"],
            &["deno", "task", "start"],
        ),
        // One ecosystem, four drivers, chosen by whichever lockfile is present —
        // running `npm install` in a pnpm workspace does not merely waste a turn,
        // it writes a second lockfile the project did not ask for.
        "package.json" => {
            // `install` and not `ci` for npm. `npm ci` is the right command in a
            // CI job and the wrong one here: it requires a lockfile and fails
            // outright without one, which is exactly what the 0.17.0 live run hit
            // on its first turn — the agent was handed a default that could not
            // work on the project in front of it. `npm install` works with a
            // lockfile and without.
            let manager = if has("bun.lockb") || has("bun.lock") {
                "bun"
            } else if has("pnpm-lock.yaml") {
                "pnpm"
            } else if has("yarn.lock") {
                "yarn"
            } else {
                "npm"
            };
            let install = "install";
            tc(
                "node",
                marker,
                manager,
                &[manager, install],
                &[manager, "run", "build"],
                &[manager, "test"],
                &[manager, "run", "lint"],
                &[manager, "run", "format"],
                &[manager, "start"],
            )
        }
        "go.mod" => tc(
            "go",
            marker,
            "go",
            &[],
            &["go", "build", "./..."],
            &["go", "test", "./..."],
            &["go", "vet", "./..."],
            &["gofmt", "-w", "."],
            &["go", "run", "."],
        ),
        "pyproject.toml" | "requirements.txt" => {
            // uv and poetry both put the interpreter behind their own runner, and
            // a bare `pytest` in a poetry project runs whatever is on PATH — which
            // is the system Python about as often as it is the project's.
            if has("uv.lock") {
                tc(
                    "python",
                    marker,
                    "uv",
                    &["uv", "sync"],
                    &[],
                    &["uv", "run", "pytest"],
                    &["uv", "run", "ruff", "check"],
                    &["uv", "run", "ruff", "format"],
                    &["uv", "run", "python", "-m", "main"],
                )
            } else if has("poetry.lock") {
                tc(
                    "python",
                    marker,
                    "poetry",
                    &["poetry", "install"],
                    &[],
                    &["poetry", "run", "pytest"],
                    &["poetry", "run", "ruff", "check"],
                    &["poetry", "run", "ruff", "format"],
                    &["poetry", "run", "python", "-m", "main"],
                )
            } else {
                tc(
                    "python",
                    marker,
                    "pip",
                    &["python", "-m", "pip", "install", "-e", "."],
                    &[],
                    &["python", "-m", "pytest"],
                    &["python", "-m", "ruff", "check"],
                    &["python", "-m", "ruff", "format"],
                    &["python", "-m", "main"],
                )
            }
        }
        "pom.xml" => tc(
            "maven",
            marker,
            "mvn",
            &["mvn", "-B", "dependency:go-offline"],
            &["mvn", "-B", "compile"],
            &["mvn", "-B", "test"],
            &[],
            &["mvn", "-B", "formatter:format"],
            &["mvn", "-B", "exec:java"],
        ),
        "build.gradle" | "build.gradle.kts" => tc(
            "gradle",
            marker,
            "gradle",
            &[],
            &["gradle", "build"],
            &["gradle", "test"],
            &["gradle", "check"],
            &[],
            &["gradle", "run"],
        ),
        "mix.exs" => tc(
            "elixir",
            marker,
            "mix",
            &["mix", "deps.get"],
            &["mix", "compile"],
            &["mix", "test"],
            &["mix", "credo"],
            &["mix", "format"],
            &["mix", "run"],
        ),
        "Gemfile" => tc(
            "ruby",
            marker,
            "bundler",
            &["bundle", "install"],
            &[],
            &["bundle", "exec", "rake", "test"],
            &["bundle", "exec", "rubocop"],
            &["bundle", "exec", "rubocop", "-a"],
            &["bundle", "exec", "ruby", "main.rb"],
        ),
        "composer.json" => tc(
            "php",
            marker,
            "composer",
            &["composer", "install"],
            &[],
            &["composer", "test"],
            &["composer", "lint"],
            &[],
            &["php", "-S", "127.0.0.1:8000"],
        ),
        "Package.swift" => tc(
            "swift",
            marker,
            "swift",
            &["swift", "package", "resolve"],
            &["swift", "build"],
            &["swift", "test"],
            &[],
            &["swift-format", "-i", "-r", "Sources"],
            &["swift", "run"],
        ),
        "CMakeLists.txt" => tc(
            "cmake",
            marker,
            "cmake",
            &[],
            &["cmake", "--build", "build"],
            &["ctest", "--test-dir", "build"],
            &[],
            &[],
            &[],
        ),
        "*.csproj" => tc(
            "dotnet",
            marker,
            "dotnet",
            &["dotnet", "restore"],
            &["dotnet", "build"],
            &["dotnet", "test"],
            &["dotnet", "format", "--verify-no-changes"],
            &["dotnet", "format"],
            &["dotnet", "run"],
        ),
        // Last, and correctly so: a Makefile beside a Cargo.toml is a convenience
        // wrapper, not the project's identity.
        _ => tc(
            "make",
            marker,
            "make",
            &[],
            &["make"],
            &["make", "test"],
            &["make", "lint"],
            &["make", "format"],
            &["make", "run"],
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher homes are read-execute grants for a payload, so the one
    /// property worth a test is what is **not** in them: `CARGO_HOME` holds
    /// `credentials.toml`, and this list must never be the thing that hands a
    /// registry token to a contained command. Asserted so that adding it for
    /// convenience has to argue with a test.
    ///
    /// Everything returned must also be an absolute directory that exists — the
    /// filter is here rather than at the one caller, which would otherwise have
    /// to know which of these are conventions and which are environment.
    #[test]
    fn the_launcher_homes_exclude_cargo_home_and_are_all_real_directories() {
        let homes = Toolchain::launcher_homes();
        for p in &homes {
            assert!(p.is_absolute() && p.is_dir(), "{p:?}");
            assert!(
                !p.ends_with(".cargo"),
                "CARGO_HOME holds credentials.toml and is read-execute here: {p:?}"
            );
        }
    }

    /// A directory holding exactly the named files, all empty.
    fn project(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        dir
    }

    /// Every marker in the table, with the ecosystem and test argv it must
    /// produce. Written out rather than derived, so a wrong entry in `detect` is
    /// a failing test and not a matching typo in two places.
    #[test]
    fn every_marker_names_its_ecosystem_and_its_test_command() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("Cargo.toml", "cargo", &["cargo", "test"]),
            ("deno.json", "deno", &["deno", "test"]),
            ("deno.jsonc", "deno", &["deno", "test"]),
            ("package.json", "node", &["npm", "test"]),
            ("go.mod", "go", &["go", "test", "./..."]),
            ("pyproject.toml", "python", &["python", "-m", "pytest"]),
            ("requirements.txt", "python", &["python", "-m", "pytest"]),
            ("pom.xml", "maven", &["mvn", "-B", "test"]),
            ("build.gradle", "gradle", &["gradle", "test"]),
            ("build.gradle.kts", "gradle", &["gradle", "test"]),
            ("mix.exs", "elixir", &["mix", "test"]),
            ("Gemfile", "ruby", &["bundle", "exec", "rake", "test"]),
            ("composer.json", "php", &["composer", "test"]),
            ("Package.swift", "swift", &["swift", "test"]),
            ("CMakeLists.txt", "cmake", &["ctest", "--test-dir", "build"]),
            ("app.csproj", "dotnet", &["dotnet", "test"]),
            ("Makefile", "make", &["make", "test"]),
        ];
        // Every ecosystem the contract names has a case here.
        assert_eq!(cases.len(), 17);

        for (marker, ecosystem, test) in cases {
            let dir = project(&[marker]);
            let found = detect(dir.path())
                .unwrap_or_else(|| panic!("{marker} must be detected as {ecosystem}"));
            assert_eq!(&found.ecosystem, ecosystem, "{marker}");
            assert_eq!(found.test, *test, "{marker}");
            assert!(
                !found.test.is_empty(),
                "{marker}: every ecosystem here has a test command"
            );
        }
    }

    #[test]
    fn the_lockfile_beside_package_json_chooses_the_package_manager() {
        for (lockfile, manager, install) in [
            ("bun.lockb", "bun", "install"),
            ("bun.lock", "bun", "install"),
            ("pnpm-lock.yaml", "pnpm", "install"),
            ("yarn.lock", "yarn", "install"),
            ("package-lock.json", "npm", "install"),
        ] {
            let dir = project(&["package.json", lockfile]);
            let found = detect(dir.path()).unwrap();
            assert_eq!(found.manager, manager, "{lockfile}");
            assert_eq!(found.test, [manager, "test"], "{lockfile}");
            assert_eq!(found.install, [manager, install], "{lockfile}");
        }
    }

    /// The negative control: nothing to go on is reported as nothing.
    #[test]
    fn a_directory_with_no_marker_reports_no_detection_rather_than_guessing() {
        let dir = project(&["README.md", "notes.txt", "src"]);
        assert_eq!(detect(dir.path()), None);
    }

    #[test]
    fn a_makefile_beside_a_real_build_system_does_not_win() {
        let dir = project(&["Makefile", "Cargo.toml"]);
        assert_eq!(detect(dir.path()).unwrap().ecosystem, "cargo");

        // And on its own it is still the answer.
        let alone = project(&["Makefile"]);
        assert_eq!(detect(alone.path()).unwrap().ecosystem, "make");
    }

    #[test]
    fn a_deno_project_carrying_a_package_json_is_still_deno() {
        let dir = project(&["package.json", "deno.json"]);
        assert_eq!(detect(dir.path()).unwrap().ecosystem, "deno");
    }

    #[test]
    fn the_python_runner_follows_the_lockfile() {
        for (lockfile, manager) in [("uv.lock", "uv"), ("poetry.lock", "poetry")] {
            let dir = project(&["pyproject.toml", lockfile]);
            let found = detect(dir.path()).unwrap();
            assert_eq!(found.manager, manager);
            assert_eq!(found.test, [manager, "run", "pytest"]);
        }
        // Neither: the interpreter directly, which is the honest fallback.
        let plain = project(&["pyproject.toml"]);
        assert_eq!(detect(plain.path()).unwrap().manager, "pip");
    }

    #[test]
    fn the_description_names_the_marker_and_omits_the_commands_that_do_not_exist() {
        let dir = project(&["CMakeLists.txt"]);
        let text = detect(dir.path()).unwrap().describe();
        assert!(text.contains("CMakeLists.txt"), "{text}");
        assert!(text.contains("test: ctest --test-dir build"), "{text}");
        // cmake has no conventional linter or formatter, and the model is not
        // invited to run an empty command.
        assert!(!text.contains("lint:"), "{text}");
        assert!(!text.contains("format:"), "{text}");
    }

    /// The table is shipped data whose whole purpose is to be deserialized by
    /// 0.19.0's configuration file into this same type.
    #[test]
    fn a_toolchain_round_trips_through_json() {
        let dir = project(&["Cargo.toml"]);
        let found = detect(dir.path()).unwrap();
        let json = serde_json::to_string(&found).unwrap();
        assert_eq!(serde_json::from_str::<Toolchain>(&json).unwrap(), found);
    }
}
