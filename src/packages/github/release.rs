use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    time::Duration,
};

use glob::Pattern;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use toml_edit::Item;
use ureq::{
    Agent, Body, Error,
    http::{Response, Uri},
};

use crate::{Ctx, Fail, run::Run};

use super::{Platform, digest_hex, failure, repo_name, tool_options};

const MAX_METADATA: u64 = 4 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;

pub(super) struct Client {
    agent: Agent,
    token: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Release {
    url: String,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
pub(super) struct Asset {
    pub name: String,
    id: u64,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

impl Client {
    pub fn new(ctx: &Ctx) -> Self {
        let token = ["GITHUB_TOKEN", "GH_TOKEN"]
            .into_iter()
            .filter_map(|name| ctx.env.get(name))
            .find(|token| !token.is_empty())
            .map(|token| token.to_string_lossy().into_owned());
        let agent = Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent("dots")
            .build()
            .into();
        Self { agent, token }
    }

    pub fn latest(&self, repo: &str, run: &Run<'_>) -> Result<(String, Release), Fail> {
        check_cancellation(run)?;
        let response = self.get(
            &format!("https://api.github.com/repos/{repo}/releases/latest"),
            "application/vnd.github+json",
            self.token.as_deref(),
        ).map_err(|err| match err {
            Error::StatusCode(404) => failure(format!("{repo} has no accessible stable GitHub release; source builds are not supported")),
            Error::StatusCode(status) => failure(format!("GitHub release lookup for {repo} failed (HTTP {status}); check GitHub access")),
            _ => failure(format!("could not reach the GitHub release API for {repo}")),
        })?;
        let mut metadata = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_METADATA + 1)
            .read_to_end(&mut metadata)
            .map_err(|err| {
                failure(format!(
                    "could not read GitHub release metadata for {repo}: {err}"
                ))
            })?;
        check_cancellation(run)?;
        if metadata.len() as u64 > MAX_METADATA {
            return Err(failure(
                "GitHub release metadata exceeds the inspection limit",
            ));
        }
        let release: Release = serde_json::from_slice(&metadata)
            .map_err(|err| failure(format!("invalid GitHub release metadata for {repo}: {err}")))?;
        if release.draft || release.prerelease || release.tag_name.is_empty() {
            return Err(failure(format!(
                "{repo} has no accessible stable GitHub release"
            )));
        }
        if release.assets.is_empty() {
            return Err(failure(format!(
                "{repo} publishes no downloadable release assets; source builds are not supported"
            )));
        }
        let canonical = canonical_repo(&release.url)
            .ok_or_else(|| failure("release metadata lacks an official GitHub release API URL"))?;
        Ok((canonical, release))
    }

    pub fn download(&self, repo: &str, asset: &Asset, run: &Run<'_>) -> Result<File, Fail> {
        check_cancellation(run)?;
        validate_download_url(repo, &asset.browser_download_url)?;
        if asset.size > MAX_ARCHIVE {
            return Err(failure(
                "release archive exceeds the 512 MiB inspection limit",
            ));
        }
        let (url, accept) = if self.token.is_some() {
            if asset.id == 0 {
                return Err(failure("release asset lacks a valid GitHub asset ID"));
            }
            (
                format!(
                    "https://api.github.com/repos/{repo}/releases/assets/{}",
                    asset.id
                ),
                "application/octet-stream",
            )
        } else {
            (
                asset.browser_download_url.clone(),
                "application/octet-stream",
            )
        };

        let response = self
            .get(&url, accept, self.token.as_deref())
            .map_err(|err| match err {
                Error::StatusCode(status) => failure(format!(
                    "GitHub release archive download failed (HTTP {status})"
                )),
                _ => failure("could not download the GitHub release archive"),
            })?;
        let mut reader = response.into_body().into_reader();
        let mut file = tempfile::tempfile().map_err(archive_io)?;
        let mut buffer = [0; 64 * 1024];
        let mut size = 0;
        let mut digest = Sha256::new();
        loop {
            check_cancellation(run)?;
            let count = reader.read(&mut buffer).map_err(archive_io)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if size > MAX_ARCHIVE {
                return Err(failure(
                    "release archive exceeds the 512 MiB inspection limit",
                ));
            }
            digest.update(&buffer[..count]);
            file.write_all(&buffer[..count]).map_err(archive_io)?;
        }
        if size != asset.size {
            return Err(failure(
                "GitHub release archive size differs from its metadata",
            ));
        }
        if let Some(expected) = asset
            .digest
            .as_deref()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            && !expected.eq_ignore_ascii_case(&digest_hex(digest.finalize()))
        {
            return Err(failure("GitHub release archive checksum mismatch"));
        }
        file.rewind().map_err(archive_io)?;
        Ok(file)
    }

    fn get(&self, url: &str, accept: &str, token: Option<&str>) -> Result<Response<Body>, Error> {
        let request = self.agent.get(url).header("Accept", accept);
        // ureq strips Authorization on redirects, including signed storage URLs.
        let request = if let Some(token) = token {
            request.header("Authorization", format!("Bearer {token}"))
        } else {
            request
        };
        request.call()
    }
}

fn check_cancellation(run: &Run<'_>) -> Result<(), Fail> {
    if run.cancelled() {
        return Err(Fail::new(130, "GitHub release resolution interrupted"));
    }
    Ok(())
}

fn archive_io(error: io::Error) -> Fail {
    failure(format!(
        "could not read or stage the GitHub release archive: {error}"
    ))
}

fn canonical_repo(url: &str) -> Option<String> {
    let tail = url.strip_prefix("https://api.github.com/repos/")?;
    let (repo, id) = tail.rsplit_once("/releases/")?;
    (id.parse::<u64>().ok()? > 0).then_some(())?;
    repo_name(repo)
}

fn validate_download_url(repo: &str, url: &str) -> Result<(), Fail> {
    let invalid = || failure("asset is not an official GitHub release download");
    let uri: Uri = url.parse().map_err(|_| invalid())?;
    let parts: Vec<_> = uri.path().split('/').collect();
    if uri.scheme_str() != Some("https")
        || uri
            .authority()
            .is_none_or(|host| !host.as_str().eq_ignore_ascii_case("github.com"))
        || uri.query().is_some()
        || !matches!(parts.as_slice(),
            ["", owner, name, "releases", "download", tag, file]
                if format!("{owner}/{name}").eq_ignore_ascii_case(repo)
                    && !matches!(*tag, "" | "." | "..")
                    && !matches!(*file, "" | "." | "..")
        )
    {
        return Err(invalid());
    }
    Ok(())
}

impl Release {
    pub fn select(&self, pattern: Option<&Pattern>, platform: Platform) -> Result<&Asset, Fail> {
        let mut candidates = Vec::new();
        if let Some(pattern) = pattern {
            candidates.extend(
                self.assets
                    .iter()
                    .filter(|asset| pattern.matches(&asset.name)),
            );
        } else {
            let mut best = None;
            for asset in &self.assets {
                let Some(score) = score(&asset.name, platform) else {
                    continue;
                };
                if best.is_none_or(|best| score > best) {
                    candidates.clear();
                    best = Some(score);
                }
                if best == Some(score) {
                    candidates.push(asset);
                }
            }
            let zip_stems: Vec<_> = candidates
                .iter()
                .filter_map(|asset| {
                    asset
                        .name
                        .to_ascii_lowercase()
                        .strip_suffix(".zip")
                        .map(str::to_owned)
                })
                .collect();
            candidates.retain(|asset| {
                !asset
                    .name
                    .to_ascii_lowercase()
                    .strip_suffix(".dmg")
                    .is_some_and(|stem| zip_stems.iter().any(|zip| zip == stem))
            });
        }
        let [asset] = candidates.as_slice() else {
            let names = if candidates.is_empty() {
                self.assets.iter().collect::<Vec<_>>()
            } else {
                candidates
            };
            let names = names
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(failure(format!(
                "cannot choose one release asset; use --asset NAME or GLOB. Available: {names}"
            )));
        };
        if matches!(asset.name.as_str(), "" | "." | "..")
            || !asset
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.+ -".contains(&byte))
        {
            return Err(failure(
                "release asset name contains unsupported characters",
            ));
        }
        Ok(asset)
    }

    pub fn declaration(
        &self,
        asset: &Asset,
        explicit: Option<&str>,
        platform: Platform,
        hook: Option<String>,
    ) -> Item {
        let (prefix, version) = tag_version(&self.tag_name);
        let automatic = [self.tag_name.as_str(), version]
            .into_iter()
            .filter(|candidate| !candidate.is_empty())
            .find_map(|candidate| wildcard_version(&asset.name, candidate))
            .filter(|pattern| {
                Pattern::new(pattern).is_ok_and(|pattern| {
                    self.assets
                        .iter()
                        .filter(|asset| pattern.matches(&asset.name))
                        .count()
                        == 1
                })
            });
        // mise handles '*' and '?' as wildcards and interprets '+' as a
        // regex metacharacter. Give it only the common wildcard subset or
        // an exact spelling of the selected asset.
        let portable_override = explicit.filter(|pattern| {
            pattern
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_. -?*".contains(&byte))
        });
        let pattern = match explicit {
            Some(_) => portable_override.unwrap_or(&asset.name),
            None => automatic.as_deref().unwrap_or(&asset.name),
        };
        let mise_pattern = pattern.replace('+', "[+]");
        tool_options(&mise_pattern, prefix, explicit.is_none(), platform, hook)
    }
}

fn score(name: &str, platform: Platform) -> Option<u8> {
    let name = name
        .to_ascii_lowercase()
        .replace("x86_64", "amd64")
        .replace("x86-64", "amd64");
    let words: Vec<_> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |names: &[&str]| names.iter().any(|token| words.contains(token));
    let os_families: &[(&str, &[&str])] = &[
        ("macos", &["macos", "darwin", "osx", "mac"]),
        (
            "linux",
            &[
                "linux",
                "ubuntu",
                "debian",
                "alpine",
                "fedora",
                "centos",
                "rhel",
                "archlinux",
                "opensuse",
                "suse",
                "nixos",
                "gentoo",
                "rockylinux",
                "almalinux",
            ],
        ),
        ("windows", &["windows", "win", "win32", "win64"]),
        ("freebsd", &["freebsd"]),
        ("netbsd", &["netbsd"]),
        ("openbsd", &["openbsd"]),
        ("android", &["android"]),
    ];
    let arch_families: &[(&str, &[&str])] = &[
        ("aarch64", &["aarch64", "arm64"]),
        ("x86_64", &["amd64", "x64"]),
        (
            "arm",
            &["arm", "armv6", "armv6l", "armv7", "armv7l", "armhf"],
        ),
        ("x86", &["x86", "i386", "i486", "i586", "i686", "386"]),
        (
            "other",
            &[
                "riscv64",
                "ppc64",
                "ppc64le",
                "powerpc64",
                "powerpc64le",
                "s390x",
                "loongarch64",
                "loong64",
                "mips",
                "mipsel",
                "mipsle",
                "mips64",
                "mips64el",
                "mips64le",
                "sparc",
                "sparc64",
            ],
        ),
    ];
    if has(&[
        "source",
        "sources",
        "checksum",
        "checksums",
        "sha256",
        "sha512",
        "debug",
        "symbols",
        "sbom",
        "intoto",
        "provenance",
    ]) || [
        ".sig", ".asc", ".msi", ".exe", ".deb", ".rpm", ".apk", ".pkg",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
        || (name.ends_with(".dmg") && platform.os != "macos")
        || os_families
            .iter()
            .any(|(os, tokens)| *os != platform.os && has(tokens))
    {
        return None;
    }
    let native_arch = arch_families
        .iter()
        .any(|(arch, tokens)| *arch == platform.arch && has(tokens));
    if arch_families.iter().any(|(arch, tokens)| {
        *arch != platform.arch
            && has(tokens)
            && !(has(&["universal"]) && matches!(*arch, "x86_64" | "aarch64"))
    }) {
        return None;
    }
    let native_os = os_families
        .iter()
        .any(|(os, tokens)| *os == platform.os && has(tokens));
    Some(20 * u8::from(native_os) + 10 * u8::from(native_arch))
}

fn tag_version(tag: &str) -> (&str, &str) {
    for (index, byte) in tag.bytes().enumerate() {
        if !byte.is_ascii_digit()
            || tag
                .as_bytes()
                .get(index.wrapping_sub(1))
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            continue;
        }
        let (prefix, version) = tag.split_at(index);
        let numeric = version.split(['-', '+']).next().unwrap_or_default();
        if numeric
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
        {
            return (prefix, version);
        }
    }
    ("", "")
}

fn wildcard_version(name: &str, version: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let (start, _) = name.match_indices(version).find(|(start, _)| {
        let end = start + version.len();
        let previous = bytes.get(start.wrapping_sub(1));
        let next = bytes.get(end);
        !previous.is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
            && !next.is_some_and(u8::is_ascii_digit)
            && !(next == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit))
    })?;
    Some(format!(
        "{}*{}",
        &name[..start],
        &name[start + version.len()..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Platform = Platform {
        os: "macos",
        arch: "aarch64",
    };
    const LINUX: Platform = Platform {
        os: "linux",
        arch: "x86_64",
    };

    fn release(names: &[&str]) -> Release {
        Release {
            url: "https://api.github.com/repos/OWNER/tool/releases/42".to_owned(),
            tag_name: "release-v1.2.0".to_owned(),
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|name| Asset {
                    name: name.to_string(),
                    id: 1,
                    size: 0,
                    digest: None,
                    browser_download_url: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn selects_native_asset_and_prefers_zip_to_matching_dmg() {
        let release = release(&[
            "Tool-linux-x86-64.tar.gz",
            "Tool-linux-arm64.tar.gz",
            "Tool-macos-universal.dmg",
            "Tool-macos-universal.zip",
            "Tool-macos-universal-checksums.txt",
            "Tool-macos-x64.zip",
        ]);
        assert_eq!(
            release.select(None, MAC).unwrap().name,
            "Tool-macos-universal.zip"
        );
        assert_eq!(
            release.select(None, LINUX).unwrap().name,
            "Tool-linux-x86-64.tar.gz"
        );
    }

    #[test]
    fn refuses_ambiguous_selection_and_matches_explicit_globs() {
        let release = release(&[
            "one-macos-arm64.zip",
            "two-macos-arm64.zip",
            "other-linux-amd64.zip",
        ]);
        assert!(
            release
                .select(None, MAC)
                .err()
                .unwrap()
                .diagnostic
                .contains("--asset")
        );
        assert!(
            release
                .select(Some(&Pattern::new("*macos*").unwrap()), MAC)
                .is_err()
        );
        assert_eq!(
            release
                .select(Some(&Pattern::new("t?o-*").unwrap()), MAC)
                .unwrap()
                .name,
            "two-macos-arm64.zip"
        );
    }

    #[test]
    fn preserves_version_prefix_and_platform_specific_app_hook() {
        let release = release(&["Tool-1.2.0-macos-arm64.zip"]);
        let asset = release.select(None, MAC).unwrap();
        let item = release.declaration(
            asset,
            None,
            MAC,
            Some("dots install-release-app owner/tool".to_owned()),
        );
        assert_eq!(item["version"].as_str(), Some("latest"));
        assert_eq!(item["version_prefix"].as_str(), Some("release-v"));
        assert_eq!(
            item["platform_macos_arm64_asset_pattern"].as_str(),
            Some("Tool-*-macos-arm64.zip")
        );
        assert_eq!(item["postinstall"]["when"].as_str(), Some("always"));
        assert_eq!(item["strip_components"].as_integer(), Some(0));
        let cli = release.declaration(asset, Some("Tool-*.zip"), MAC, None);
        assert_eq!(cli["asset_pattern"].as_str(), Some("Tool-*.zip"));
        assert!(cli.get("postinstall").is_none());
    }

    #[test]
    fn declaration_matches_literal_plus_in_selected_asset() {
        let release = release(&["Tool+1.2.0-macos-arm64.zip", "Tool1.2.0-macos-arm64.zip"]);
        let explicit = "Tool+1.2.0-*.zip";
        let asset = release
            .select(Some(&Pattern::new(explicit).unwrap()), MAC)
            .unwrap();
        assert_eq!(
            release.declaration(asset, Some(explicit), MAC, None)["asset_pattern"].as_str(),
            Some("Tool[+]1.2.0-macos-arm64.zip")
        );
        assert_eq!(
            release.declaration(asset, None, MAC, None)["platform_macos_arm64_asset_pattern"]
                .as_str(),
            Some("Tool[+]*-macos-arm64.zip")
        );
    }

    #[test]
    fn checks_canonical_release_and_asset_origins() {
        let release = release(&[]);
        assert_eq!(canonical_repo(&release.url).as_deref(), Some("owner/tool"));
        assert!(
            validate_download_url(
                "owner/tool",
                "https://github.com/Owner/Tool/releases/download/v1/tool.zip"
            )
            .is_ok()
        );
        for url in [
            "https://invalid.example/owner/tool/releases/download/v1/tool.zip",
            "https://github.com/other/tool/releases/download/v1/tool.zip",
            "https://github.com/owner/tool/releases/download/v1/tool.zip?x=1",
        ] {
            assert!(validate_download_url("owner/tool", url).is_err(), "{url}");
        }
    }
}
