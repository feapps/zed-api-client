use zed_extension_api::{self as zed, settings::LspSettings};

/// Id do language server, como declarado no `extension.toml` — também é a chave
/// usada em `"lsp"` no `settings.json` do Zed.
const SERVER_ID: &str = "http-request-client";
/// Nome do binário do language server (crate `lsp-server`).
const BINARY_NAME: &str = "http-request-client-lsp";
/// Arquivo que só existe no repositório da própria extensão, usado para
/// detectar que o worktree aberto é este projeto.
const REPO_MARKER: &str = "lsp-server/Cargo.toml";

struct HttpRequestClientExtension;

impl HttpRequestClientExtension {
    /// Resolve o binário do language server, da fonte mais explícita para a mais
    /// automática:
    ///
    /// 1. `lsp.http-request-client.binary.path` no `settings.json` do Zed;
    /// 2. `target/debug/` do próprio repositório da extensão, quando é ele que
    ///    está aberto (fluxo de desenvolvimento, sem precisar instalar nada);
    /// 3. o `$PATH` — funciona depois de `cargo install --path lsp-server`.
    ///
    /// Uma extensão publicada teria aqui um passo final baixando o binário do
    /// release (`latest_github_release` + `download_file`).
    fn binary_path(&self, worktree: &zed::Worktree) -> zed::Result<String> {
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
            return Ok(format!(
                "{}/target/debug/{BINARY_NAME}",
                worktree.root_path()
            ));
        }

        if let Some(path) = worktree.which(BINARY_NAME) {
            return Ok(path);
        }

        Err(format!(
            "não encontrei o binário `{BINARY_NAME}`. Instale-o com \
             `cargo install --path lsp-server` (a partir do repositório da \
             extensão) ou aponte o caminho no settings.json do Zed:\n\
             \"lsp\": {{ \"{SERVER_ID}\": {{ \"binary\": {{ \"path\": \
             \"/caminho/para/{BINARY_NAME}\" }} }} }}"
        ))
    }
}

impl zed::Extension for HttpRequestClientExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        Ok(zed::Command {
            command: self.binary_path(worktree)?,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(HttpRequestClientExtension);
