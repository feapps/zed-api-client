use zed_extension_api::{self as zed, settings::LspSettings, LanguageServerId, Result};

/// Id do language server, como declarado no `extension.toml` — também é a chave
/// usada em `"lsp"` no `settings.json` do Zed.
const SERVER_ID: &str = "http-request-client";
/// Nome do binário do language server (crate `lsp-server`).
const BINARY_NAME: &str = "http-request-client-lsp";
/// Arquivo que só existe no repositório da própria extensão, usado para
/// detectar que o worktree aberto é este projeto.
const REPO_MARKER: &str = "lsp-server/Cargo.toml";
/// Repositório (`owner/repo`) de onde baixar o binário publicado do language
/// server, via GitHub Releases.
const RELEASE_REPO: &str = "feapps/zed-api-client";

#[derive(Default)]
struct HttpRequestClientExtension {
    /// Caminho do binário já baixado nesta sessão, para não repetir o download
    /// (nem a ida à API do GitHub) a cada `language_server_command`.
    cached_binary_path: Option<String>,
}

impl HttpRequestClientExtension {
    /// Resolve o binário do language server, da fonte mais explícita para a mais
    /// automática:
    ///
    /// 1. `lsp.http-request-client.binary.path` no `settings.json` do Zed;
    /// 2. `target/debug/` do próprio repositório da extensão, quando é ele que
    ///    está aberto (fluxo de desenvolvimento, sem precisar instalar nada);
    /// 3. o `$PATH` — funciona depois de `cargo install --path lsp-server`;
    /// 4. o binário publicado no GitHub Release (fluxo do usuário final que
    ///    instala a extensão da loja).
    fn binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        if let Some(path) = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.binary)
            .and_then(|binary| binary.path)
        {
            return Ok(path);
        }

        // O extension host roda em WASI e não consegue verificar arquivos fora
        // do diretório da extensão; por isso a detecção é indireta, lendo um
        // arquivo do worktree que só existe neste repositório.
        if worktree.read_text_file(REPO_MARKER).is_ok() {
            return Ok(format!("{}/target/debug/{BINARY_NAME}", worktree.root_path()));
        }

        if let Some(path) = worktree.which(BINARY_NAME) {
            return Ok(path);
        }

        self.download_binary(language_server_id)
    }

    /// Baixa o binário do language server do último GitHub Release de
    /// [`RELEASE_REPO`], para a plataforma atual, e devolve o caminho.
    ///
    /// O binário fica num diretório versionado dentro da pasta de trabalho da
    /// extensão; se o dessa versão já existe, é reaproveitado sem baixar de
    /// novo. Versões antigas são removidas após um download novo.
    ///
    /// Convenção de nome do asset (o workflow de release precisa produzir
    /// exatamente isto — um gzip do binário cru):
    ///
    /// ```text
    /// http-request-client-lsp-<os>-<arch>.gz
    ///   <os>   = macos | linux | windows
    ///   <arch> = aarch64 | x86_64 | x86
    /// ```
    fn download_binary(&mut self, language_server_id: &LanguageServerId) -> Result<String> {
        // Reaproveita o que já foi baixado nesta sessão, se ainda está no disco.
        if let Some(path) = &self.cached_binary_path {
            if std::fs::metadata(path).is_ok_and(|stat| stat.is_file()) {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            RELEASE_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (os, arch) = zed::current_platform();
        let os_name = match os {
            zed::Os::Mac => "macos",
            zed::Os::Linux => "linux",
            zed::Os::Windows => "windows",
        };
        let arch_name = match arch {
            zed::Architecture::Aarch64 => "aarch64",
            zed::Architecture::X8664 => "x86_64",
            zed::Architecture::X86 => "x86",
        };
        let asset_name = format!("{BINARY_NAME}-{os_name}-{arch_name}.gz");

        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                let available = release
                    .assets
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "release {} has no `{asset_name}` asset for this platform. \
                     Available assets: [{available}]",
                    release.version
                )
            })?;

        let version_dir = format!("{BINARY_NAME}-{}", release.version);
        let binary_ext = if matches!(os, zed::Os::Windows) { ".exe" } else { "" };
        let binary_path = format!("{version_dir}/{BINARY_NAME}{binary_ext}");

        if !std::fs::metadata(&binary_path).is_ok_and(|stat| stat.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            std::fs::create_dir_all(&version_dir)
                .map_err(|e| format!("failed to create {version_dir}: {e}"))?;
            zed::download_file(
                &asset.download_url,
                &binary_path,
                zed::DownloadedFileType::Gzip,
            )
            .map_err(|e| format!("failed to download {}: {e}", asset.download_url))?;
            zed::make_file_executable(&binary_path)?;

            // Remove instalações de versões anteriores.
            if let Ok(entries) = std::fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(BINARY_NAME) && name != version_dir {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for HttpRequestClientExtension {
    fn new() -> Self {
        Self::default()
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = self
            .binary_path(language_server_id, worktree)
            .map_err(|e| {
                // Deixa o motivo visível na UI do Zed em vez de só falhar em silêncio.
                zed::set_language_server_installation_status(
                    language_server_id,
                    &zed::LanguageServerInstallationStatus::Failed(e.clone()),
                );
                e
            })?;
        Ok(zed::Command {
            command,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(HttpRequestClientExtension);
