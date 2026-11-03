use framer::Project;
use framer::toml_edit::{Array, Value};
use framer_presets::{Dist, Release, ReleasePlz, Rust, RustMonorepo, Workspace};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut project = Project::new();
    project.apply(
        RustMonorepo::new()
            .rust(Rust::new("1.98"))
            .workspace(
                Workspace::new()
                    .members(["crates/*", ".framer"])
                    .repository("https://github.com/jcarver989/clankervm"),
            )
            .release(
                Release::new()
                    .release_plz(ReleasePlz::new().set("git_only", true))
                    .dist(
                        Dist::new()
                            .targets([
                                "aarch64-apple-darwin",
                                "aarch64-unknown-linux-gnu",
                                "x86_64-apple-darwin",
                                "x86_64-unknown-linux-gnu",
                            ])
                            .homebrew_tap("jcarver989/homebrew-tap"),
                    ),
            ),
    );
    let doc_valid_idents: Array = ["ClankerVM", "MicroVM", "MicroVMs", ".."]
        .into_iter()
        .map(Value::from)
        .collect();
    project
        .toml("clippy.toml")
        .set("doc-valid-idents", doc_valid_idents);
    project.synth()
}
