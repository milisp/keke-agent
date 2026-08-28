//! `keke mcp`, run as the binary a person runs.
//!
//! What is asserted here is the property the feature exists for: a server named
//! on the command line is afterwards a configured server, in a file keke's
//! ordinary plugin discovery reads — not in a store only this command knows
//! about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let workspace = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join("config.toml"), "provider = \"grok\"\n").expect("write");
        Self { home, workspace }
    }

    fn keke(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_keke"));
        command
            .env("KEKE_HOME", self.home.path())
            .env("KEKE_CREDENTIAL_STORE", "file")
            .env("KEKE_IMPORT", "off")
            .arg("--cwd")
            .arg(self.workspace.path());
        command
    }

    /// Run to completion, returning stdout and stderr together — a failure
    /// message is as much part of the behavior as a success one.
    fn run(&self, args: &[&str]) -> (bool, String) {
        let output = self.keke().args(args).output().expect("runs");
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }
}

#[test]
fn a_remote_server_is_added_listed_and_removed() {
    let fixture = Fixture::new();

    let (ok, output) = fixture.run(&[
        "mcp",
        "add",
        "--transport",
        "http",
        "vercel",
        "https://mcp.vercel.com",
    ]);
    assert!(ok, "{output}");

    // The file it wrote is the ecosystem's, in the person's own directory.
    let written =
        std::fs::read_to_string(fixture.home.path().join(".mcp.json")).expect("the file exists");
    let json: serde_json::Value = serde_json::from_str(&written).expect("json");
    assert_eq!(json["mcpServers"]["vercel"]["type"], "http");
    assert_eq!(
        json["mcpServers"]["vercel"]["url"],
        "https://mcp.vercel.com"
    );

    let (ok, output) = fixture.run(&["mcp", "list"]);
    assert!(ok, "{output}");
    assert!(output.contains("vercel"), "{output}");
    assert!(output.contains("https://mcp.vercel.com"), "{output}");

    let (ok, output) = fixture.run(&["mcp", "get", "vercel"]);
    assert!(ok, "{output}");
    assert!(output.contains("transport: http"), "{output}");

    let (ok, output) = fixture.run(&["mcp", "remove", "vercel"]);
    assert!(ok, "{output}");
    let (_, output) = fixture.run(&["mcp", "list"]);
    assert!(!output.contains("vercel"), "{output}");
}

#[test]
fn a_stdio_server_keeps_its_whole_command_line() {
    let fixture = Fixture::new();

    let (ok, output) = fixture.run(&[
        "mcp",
        "add",
        "postgres",
        "-e",
        "DATABASE_URL=${DATABASE_URL}",
        "--",
        "npx",
        "-y",
        "@modelcontextprotocol/server-postgres",
    ]);
    assert!(ok, "{output}");

    let (_, output) = fixture.run(&["mcp", "get", "postgres"]);
    assert!(
        output.contains("npx -y @modelcontextprotocol/server-postgres"),
        "{output}"
    );
    // A reference is what was stored, and only its name is ever printed.
    assert!(output.contains("env: DATABASE_URL"), "{output}");
    assert!(!output.contains("${DATABASE_URL}"), "{output}");
}

#[test]
fn adding_the_same_name_twice_refuses_rather_than_replacing() {
    let fixture = Fixture::new();
    let add = ["mcp", "add", "--transport", "http", "api", "https://a.test"];

    assert!(fixture.run(&add).0);
    let (ok, output) = fixture.run(&add);
    assert!(!ok, "{output}");
    assert!(output.contains("--force"), "{output}");

    let (ok, _) = fixture.run(&[
        "mcp",
        "add",
        "--force",
        "--transport",
        "http",
        "api",
        "https://b.test",
    ]);
    assert!(ok);
    let (_, output) = fixture.run(&["mcp", "get", "api"]);
    assert!(output.contains("https://b.test"), "{output}");
}

#[test]
fn a_server_the_project_configures_is_held_back_until_it_is_trusted() {
    let fixture = Fixture::new();

    let (ok, output) = fixture.run(&[
        "mcp",
        "add",
        "--scope",
        "project",
        "--transport",
        "http",
        "shipped",
        "https://mcp.example.test",
    ]);
    assert!(ok, "{output}");
    assert!(
        fixture.workspace.path().join(".keke/.mcp.json").is_file(),
        "it belongs to the project, not to the person"
    );

    let (_, output) = fixture.run(&["mcp", "list"]);
    assert!(output.contains("not trusted"), "{output}");

    assert!(fixture.run(&["plugin", "trust", "workspace"]).0);
    let (_, output) = fixture.run(&["mcp", "list"]);
    assert!(!output.contains("not trusted"), "{output}");
}

#[test]
fn a_transport_and_its_arguments_have_to_agree() {
    let fixture = Fixture::new();

    let (ok, output) = fixture.run(&["mcp", "add", "--transport", "http", "api"]);
    assert!(!ok, "{output}");
    assert!(output.contains("needs a URL"), "{output}");

    let (ok, output) = fixture.run(&["mcp", "add", "api"]);
    assert!(!ok, "{output}");
    assert!(output.contains("needs a command"), "{output}");
}

#[test]
fn a_command_file_in_the_persons_own_directory_becomes_a_slash_command() {
    let fixture = Fixture::new();
    let commands = fixture.home.path().join("commands");
    std::fs::create_dir_all(&commands).expect("mkdir");
    std::fs::write(
        commands.join("review.md"),
        "---\ndescription: review the diff\n---\n\nReview what has changed.\n",
    )
    .expect("write");

    // `plugin list` is where contributions are reported, and a person's own
    // directory is read as one — which is what makes the command available
    // without authoring a plugin.
    let (ok, output) = fixture.run(&["plugin", "list"]);
    assert!(ok, "{output}");
    assert!(output.contains("local"), "{output}");
    assert!(output.contains("1 commands"), "{output}");
}
