// Copyright 2020 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use cargo_metadata::Metadata;
use flate2::Compression;
use httpmock::{Method::GET, MockRef, MockServer};
use indoc::{formatdoc, indoc};
use serde_json::json;
use tempfile::TempDir;

use std::{
  collections::HashMap,
  fs::{create_dir_all, write, File},
  io::Write,
  path::Path,
};

use crate::{
  metadata::{
    tests::{dummy_raze_metadata_fetcher, DummyCargoMetadataFetcher},
    RazeMetadata,
  },
  util::package_ident,
};

/// A module containing constants for each metadata template
pub mod templates {
  pub const BASIC_METADATA: &str = "basic_metadata.json.template";
  pub const DUMMY_BINARY_DEPENDENCY_REMOTE: &str =  "dummy_binary_dependency_remote.json.template";
  pub const DUMMY_MODIFIED_METADATA: &str =  "dummy_modified_metadata.json.template";
  pub const DUMMY_WORKSPACE_MEMBERS_METADATA: &str =  "dummy_workspace_members_metadata.json.template";
  pub const PLAN_BUILD_PRODUCES_ALIASED_DEPENDENCIES: &str =  "plan_build_produces_aliased_dependencies.json.template";
  pub const PLAN_BUILD_PRODUCES_BUILD_PROC_MACRO_DEPENDENCIES: &str =  "plan_build_produces_build_proc_macro_dependencies.json.template";
  pub const PLAN_BUILD_PRODUCES_PROC_MACRO_DEPENDENCIES: &str =  "plan_build_produces_proc_macro_dependencies.json.template";
  pub const SEMVER_MATCHING: &str =  "semver_matching.json.template";
  pub const SUBPLAN_PRODUCES_CRATE_ROOT_WITH_FORWARD_SLASH: &str =  "subplan_produces_crate_root_with_forward_slash.json.template";
}

pub const fn basic_toml_contents() -> &'static str {
  indoc! { r#"
    [package]
    name = "test"
    version = "0.0.1"
  
    [lib]
    path = "not_a_file.rs"
  "# }
}

pub const fn basic_lock_contents() -> &'static str {
  indoc! { r#"
    [[package]]
    name = "test"
    version = "0.0.1"
    dependencies = [
    ]
  "# }
}

pub const fn advanced_toml_contents() -> &'static str {
  indoc! { r#"
    [package]
    name = "cargo-raze-test"
    version = "0.1.0"

    [lib]
    path = "not_a_file.rs"

    [dependencies]
    proc-macro2 = "1.0.24"
  "# }
}

pub const fn advanced_lock_contents() -> &'static str {
  indoc! { r#"
    # This file is automatically @generated by Cargo.
    # It is not intended for manual editing.
    [[package]]
    name = "cargo-raze-test"
    version = "0.1.0"
    dependencies = [
      "proc-macro2",
    ]

    [[package]]
    name = "proc-macro2"
    version = "1.0.24"
    source = "registry+https://github.com/rust-lang/crates.io-index"
    checksum = "1e0704ee1a7e00d7bb417d0770ea303c1bccbabf0ef1667dae92b5967f5f8a71"
    dependencies = [
      "unicode-xid",
    ]

    [[package]]
    name = "unicode-xid"
    version = "0.2.1"
    source = "registry+https://github.com/rust-lang/crates.io-index"
    checksum = "f7fe0bb3479651439c9112f72b6c505038574c9fbb575ed1bf3b797fa39dd564"
  "# }
}

pub fn named_toml_contents(name: &str, version: &str) -> String {
  formatdoc! { r#"
    [package]
    name = "{name}"
    version = "{version}"

    [lib]
    path = "not_a_file.rs"

  "#, name = name, version = version }
}

pub fn named_lock_contents(name: &str, version: &str) -> String {
  formatdoc! { r#"
    [[package]]
    name = "{name}"
    version = "{version}"

    dependencies = [
    ]

  "#, name = name, version = version }
}

pub fn make_workspace(toml_file: &str, lock_file: Option<&str>) -> TempDir {
  let dir = TempDir::new().unwrap();
  // Create Cargo.toml
  {
    let path = dir.path().join("Cargo.toml");
    let mut toml = File::create(&path).unwrap();
    toml.write_all(toml_file.as_bytes()).unwrap();
  }

  if let Some(lock_file) = lock_file {
    let path = dir.path().join("Cargo.lock");
    let mut lock = File::create(&path).unwrap();
    lock.write_all(lock_file.as_bytes()).unwrap();
  }

  File::create(dir.as_ref().join("WORKSPACE.bazel")).unwrap();
  dir
}

pub fn make_basic_workspace() -> TempDir {
  make_workspace(basic_toml_contents(), Some(basic_lock_contents()))
}

pub fn make_workspace_with_dependency() -> TempDir {
  make_workspace(advanced_toml_contents(), Some(advanced_lock_contents()))
}

/// A helper stuct for mocking a crates.io remote crate endpoint
pub struct MockRemoteCrateInfo<'http_mock_server> {
  // A directory of mock data to pull via a mocked endpoint
  pub data_dir: TempDir,
  // mocked endpoints
  pub endpoints: Vec<MockRef<'http_mock_server>>,
}

/// Configures the given mock_server (representing a crates.io remote) to return
/// mock responses for the given crate and version .
pub fn mock_remote_crate<'server>(
  name: &str,
  version: &str,
  mock_server: &'server MockServer,
) -> MockRemoteCrateInfo<'server> {
  // Crate info mock response
  let mock_metadata = mock_server.mock(|when, then| {
    when.method(GET).path(format!("/api/v1/crates/{}", name));
    // Note that `crate[versions]` is an arbitrary value that must only match a `versions[id]`
    then.status(200).json_body(json!({
        "crate": {
            "id": name,
            "name": name,
            "versions": [
                123456
            ],
        },
        "versions": [
            {
                "id": 123456,
                "crate": name,
                "num": version,
                "dl_path": format!("/api/v1/crates/{}/{}/download", name, version),
            }
        ],
    }));
  });

  // Create archive contents
  let dir = tempfile::TempDir::new().unwrap();
  let tar_path = dir.as_ref().join(format!("{}.tar.gz", name));
  {
    create_dir_all(dir.as_ref().join("archive")).unwrap();
    File::create(dir.as_ref().join("archive/test")).unwrap();
    write(
      dir.as_ref().join("archive/Cargo.toml"),
      named_toml_contents(name, version),
    )
    .unwrap();
    write(
      dir.as_ref().join("archive/Cargo.lock"),
      named_lock_contents(name, version),
    )
    .unwrap();

    let tar_gz: File = File::create(&tar_path).unwrap();
    let enc = flate2::write::GzEncoder::new(tar_gz, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar
      .append_dir_all(package_ident(name, version), dir.as_ref().join("archive"))
      .unwrap();
  }

  // Create download mock response
  let mock_download = mock_server.mock(|when, then| {
    when
      .method(GET)
      .path(format!("/api/v1/crates/{}/{}/download", name, version));
    then
      .status(200)
      .header("content-type", "application/x-tar")
      .body_from_file(&tar_path.display().to_string());
  });

  MockRemoteCrateInfo {
    data_dir: dir,
    endpoints: vec![mock_metadata, mock_download],
  }
}

/// A helper macro for passing a `crates` to  `mock_crate_index`
pub fn to_index_crates_map(list: Vec<(&str, &str)>) -> HashMap<String, String> {
  list
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Create a mock cache in a temporary direcotry that contains a set of given crates
pub fn mock_crate_index(
  crates: &HashMap<String, String>,
  mock_dir: Option<&Path>,
) -> Option<TempDir> {
  let index_url_mock_dir = TempDir::new().unwrap();

  // If an existing directory is provided, use that instead
  let index_dir = match mock_dir {
    Some(dir) => dir,
    None => index_url_mock_dir.as_ref(),
  };

  for (name, version) in crates {
    let crate_index_path = if name.len() < 4 {
      index_dir.join(name.len().to_string()).join(name)
    } else {
      index_dir
        .join(&name.as_str()[0..2])
        .join(&name.as_str()[2..4])
        .join(name)
    };

    create_dir_all(crate_index_path.parent().unwrap()).unwrap();
    write(
      crate_index_path,
      json!({
        "name": name,
        "vers": version,
        "deps": [],
        "cksum": "8a648e87a02fa31d9d9a3b7c76dbfee469402fbb4af3ae98b36c099d8a82bb18",
        "features": {},
        "yanked": false,
        "links": null
      })
      .to_string(),
    )
    .unwrap();
  }

  // Return the generated TempDir in the event that `mock_dir` was not provided
  if mock_dir.is_none() {
    Some(index_url_mock_dir)
  } else {
    None
  }
}

/// Generate RazeMetadata from a cargo metadata template
pub fn template_raze_metadata(template_path: &str) -> RazeMetadata {
  let dir = make_basic_workspace();
    let (mut fetcher, _server, _index_dir) = dummy_raze_metadata_fetcher();

    // Always render basic metadata
    fetcher.set_metadata_fetcher(Box::new(DummyCargoMetadataFetcher {
      metadata_template: Some(template_path.to_string()),
    }));

    fetcher.fetch_metadata(dir.as_ref(), None, None).unwrap()
}

/// Load a cargo metadata template
pub fn template_metadata(template_path: &str) -> Metadata {
  template_raze_metadata(template_path).metadata
}
