//! HTTP Request Client — language server para arquivos `.http` (estilo REST Client).
//!
//! Fornece um Code Lens "Send request" acima de cada requisição; ao clicar,
//! o servidor faz a requisição HTTP de verdade e escreve a resposta formatada
//! num buffer de resultado (caminho fixo) que o Zed abre num painel ao lado.
//!
//! A escolha do mecanismo de abertura (applyEdit + CreateFile em caminho fixo,
//! criando só na 1ª vez e substituindo o conteúdo depois) foi validada
//! empiricamente — ver o histórico de spikes / memória do projeto.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request as LspRequest, RequestId, Response};
use lsp_types::{
    ApplyWorkspaceEditParams, CodeLens, CodeLensOptions, CodeLensParams, Command as LspCommand,
    CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, ExecuteCommandOptions,
    ExecuteCommandParams, MessageType, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    Range, ResourceOp, ServerCapabilities, ShowMessageParams, TextDocumentEdit,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Uri, WorkspaceEdit,
};
use serde_json::Value;

const CMD_SEND: &str = "http.sendRequest";
const CMD_NOOP: &str = "http.noop";
/// Pasta dos arquivos de resultado, fora do worktree — assim eles não sujam o
/// projeto nem precisam de `.gitignore`.
///
/// Já esteve documentado aqui que o arquivo *precisava* estar no worktree para
/// o file watcher do Zed recarregar o buffer. Foi testado e é falso: o
/// `applyEdit` + `CreateFile` abre a aba num caminho de `/tmp` do mesmo jeito
/// (o Zed cria um worktree invisível de arquivo único e registra o language
/// server nele) e a escrita em disco gera `didChange` normalmente — worktrees
/// de arquivo único também são observados.
const RESULT_DIR: &str = "requests";
/// Nome do arquivo de resultado quando não se sabe o workspace.
const RESULT_FALLBACK: &str = "http-response.http";
/// Tempo mínimo que o `⏳ Enviando…` fica no lugar do Code Lens.
///
/// O Zed espera 50 ms (debounce) + 30 ms antes de pedir os lenses de volta, e
/// cada `workspace/codeLens/refresh` novo *substitui* o pedido pendente em vez
/// de enfileirá-lo. Numa requisição rápida (localhost responde em poucos ms) o
/// refresh do fim cancela o do começo e o indicador nunca chega a ser
/// desenhado. Segurar o estado de loading afasta os dois refreshes o bastante
/// para o Zed renderizar o do meio — e dá tempo de o olho pegar.
const MIN_LOADING: Duration = Duration::from_millis(400);

static COUNTER: AtomicI32 = AtomicI32::new(1);
/// Serializa as escritas no arquivo de resultado.
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Arquivo de log, definido por [`init_log`] a partir da raiz do workspace.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Diretório privado desta sessão em `temp_dir()`, criado com permissão 0700 e
/// nome aleatório. Todos os artefatos (buffers de resultado e logs) vivem
/// dentro dele — assim outro usuário da máquina não consegue ler as respostas
/// (que podem conter tokens) nem plantar um symlink para desviar as escritas,
/// já que não consegue nem atravessar o diretório.
static ARTIFACT_DIR: OnceLock<PathBuf> = OnceLock::new();

fn next_n() -> i32 {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Gera um token hex aleatório (16 bytes de `/dev/urandom`; recorre a
/// pid + relógio se não der para ler). Usado no nome do diretório privado, para
/// um atacante não conseguir prever nem pré-criar o caminho.
fn random_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        if f.read_exact(&mut buf).is_ok() {
            let mut s = String::with_capacity(32);
            for b in buf {
                s.push_str(&format!("{b:02x}"));
            }
            return s;
        }
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid:x}{nanos:x}")
}

/// Cria um diretório com permissão restrita (0700 no Unix). Com
/// `recursive = false` falha se o diretório já existir — o que impede reusar um
/// diretório pré-plantado por outro usuário.
fn create_dir_restricted(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        b.mode(0o700);
    }
    b.create(path)
}

/// Abre um arquivo para escrita truncando, com permissão 0600 no Unix.
fn open_write_restricted(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    o.open(path)
}

/// Diretório privado da sessão (ver [`ARTIFACT_DIR`]). Se a criação falhar
/// (caso raríssimo), recorre a `temp_dir()` direto e avisa no stderr — não em
/// `log`, que escreve justamente aqui dentro.
fn artifact_dir() -> &'static Path {
    ARTIFACT_DIR
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("http-request-client-{}", random_token()));
            match create_dir_restricted(&dir, false) {
                Ok(()) => dir,
                Err(e) => {
                    eprintln!(
                        "http-request-client-lsp: não consegui criar {} ({e}); usando {} — \
                         artefatos podem ficar legíveis por outros usuários",
                        dir.display(),
                        std::env::temp_dir().display()
                    );
                    std::env::temp_dir()
                }
            }
        })
        .as_path()
}

/// Aponta o log para um arquivo por workspace.
///
/// O Zed sobe um language server por projeto aberto, e todos eles rodavam na
/// mesma máquina escrevendo no mesmo caminho fixo — os logs se misturavam e
/// davam a impressão de um projeto estar fazendo o que era do outro.
fn init_log(root: Option<&str>) {
    let name = root
        .and_then(|r| Path::new(r).file_name())
        .map(|n| format!("http-request-client-lsp-{}.log", n.to_string_lossy()))
        .unwrap_or_else(|| "http-request-client-lsp.log".to_string());
    let _ = LOG_PATH.set(artifact_dir().join(name));
}

fn log(msg: impl AsRef<str>) {
    let path = LOG_PATH.get_or_init(|| artifact_dir().join("http-request-client-lsp.log"));
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    if let Ok(mut f) = o.open(path) {
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}

// ---------------------------------------------------------------------------
// Estado compartilhado
// ---------------------------------------------------------------------------

/// Resposta guardada de uma requisição nomeada, para encadeamento
/// `{{nome.response.body.campo}}`, `{{nome.response.headers.Header}}` e
/// `{{nome.response.status}}`.
#[derive(Clone, Default)]
struct StoredResponse {
    /// Código de status HTTP (ex.: 200).
    status: u16,
    /// Corpo parseado como JSON (ou `Value::String` cru, se não for JSON).
    body: Value,
    /// Headers da resposta, na ordem/caixa originais (lookup é case-insensitive).
    headers: Vec<(String, String)>,
}

#[derive(Default)]
struct State {
    /// Texto dos documentos `.http` abertos (uri -> conteúdo), via didOpen/didChange.
    docs: HashMap<String, String>,
    /// Última resposta por nome de requisição, usada para encadeamento
    /// `{{nome.response.body.campo}}` e `{{nome.response.headers.Header}}`.
    ///
    /// Agrupada por "ambiente" (pasta do arquivo `.http`) → nome da requisição.
    /// Sem esse agrupamento, um `# @name login` em `.rest/hml/api.http` e outro
    /// em `.rest/prd/api.http` dividiam a mesma chave: autenticar num ambiente
    /// derrubava o token do outro. A pasta é o mesmo critério usado para achar o
    /// `.env` (ver [`load_dotenv`]), então "ambiente" quer dizer a mesma coisa
    /// nos dois lugares — e arquivos `.http` da mesma pasta continuam podendo
    /// encadear entre si.
    responses: HashMap<String, HashMap<String, StoredResponse>>,
    /// Requisições em andamento (uri, linha) — para o indicador de loading.
    inflight: HashSet<(String, u32)>,
    /// Raiz do workspace, para localizar o `.env`.
    root_path: Option<String>,
    /// Se já pedimos ao Zed para abrir a aba de resultado nesta sessão.
    ///
    /// Contabilidade própria, de propósito: usar `docs` (didOpen/didClose do
    /// cliente) para isso fazia o servidor repetir o `CreateFile` e o Zed abrir
    /// uma **segunda aba** do mesmo arquivo. Ver [`perform_request`].
    result_opened: bool,
}

type Shared = Arc<Mutex<State>>;

impl State {
    /// Texto de um documento: o buffer sincronizado por didOpen/didChange ou,
    /// na falta dele, o conteúdo em disco.
    ///
    /// O fallback existe porque os Code Lens não podem depender do
    /// bookkeeping de didOpen/didClose do cliente: basta um didClose a mais
    /// (abas de preview, o mesmo arquivo em dois painéis) para a aba ficar sem
    /// os botões "Send request" até ser reaberta.
    fn document_text(&self, uri: &str) -> Option<String> {
        if let Some(text) = self.docs.get(uri) {
            return Some(text.clone());
        }
        let path = uri.strip_prefix("file://")?;
        std::fs::read_to_string(path).ok()
    }
}

// ---------------------------------------------------------------------------
// Parsing do formato .http (REST Client)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct HttpRequest {
    name: Option<String>,
    /// Linha (0-based) do "METHOD url", onde o Code Lens é ancorado.
    line: u32,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

fn is_http_method(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "TRACE" | "CONNECT"
    )
}

/// Faz o parse do documento em variáveis de arquivo (`@nome = valor`) e requisições.
fn parse_document(text: &str) -> (HashMap<String, String>, Vec<HttpRequest>) {
    let mut file_vars = HashMap::new();
    let mut reqs = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut cur_name: Option<String> = None;
    let mut idx = 0usize;

    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim_start();

        // Separador de requisições.
        if trimmed.starts_with("###") {
            cur_name = None;
            idx += 1;
            continue;
        }

        // Variável de arquivo: @nome = valor
        if let Some(rest) = trimmed.strip_prefix('@') {
            if let Some(eq) = rest.find('=') {
                let name = rest[..eq].trim().to_string();
                let val = rest[eq + 1..].trim().to_string();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    file_vars.insert(name, val);
                    idx += 1;
                    continue;
                }
            }
        }

        // Comentário (# ou //). `# @name X` define o nome da próxima requisição.
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            let content = trimmed.trim_start_matches(['#', '/']).trim();
            if let Some(n) = content.strip_prefix("@name") {
                cur_name = Some(n.trim().to_string());
            }
            idx += 1;
            continue;
        }

        if trimmed.is_empty() {
            idx += 1;
            continue;
        }

        // Linha de requisição: METHOD url [HTTP/versão]
        let mut parts = trimmed.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        if !is_http_method(&method) {
            idx += 1;
            continue;
        }
        let mut url = parts.next().unwrap_or("").to_string();
        let req_line = idx as u32;
        idx += 1;

        // Continuações de URL (query em linhas com ? ou &) + headers, até uma
        // linha em branco. Comentários (# ou //) são ignorados nesta região.
        let mut headers = Vec::new();
        while idx < lines.len() {
            let l = lines[idx];
            let t = l.trim();
            if t.is_empty() || t.starts_with("###") {
                break;
            }
            if t.starts_with('#') || t.starts_with("//") {
                idx += 1;
                continue;
            }
            // Query string multilinha: append direto (o ?/& já vem no texto).
            if t.starts_with('?') || t.starts_with('&') {
                url.push_str(t);
                idx += 1;
                continue;
            }
            if let Some(colon) = l.find(':') {
                let k = l[..colon].trim().to_string();
                let v = l[colon + 1..].trim().to_string();
                if !k.is_empty() {
                    headers.push((k, v));
                }
            }
            idx += 1;
        }

        // Pula uma linha em branco separando headers do body.
        if idx < lines.len() && lines[idx].trim().is_empty() {
            idx += 1;
        }

        // Body até o próximo ### (ou fim). Comentários (# ou //) são ignorados
        // — inclusive os de documentação após o corpo, que senão iriam para o
        // corpo da requisição.
        let mut body_lines = Vec::new();
        while idx < lines.len() {
            let l = lines[idx];
            let t = l.trim_start();
            if t.starts_with("###") {
                break;
            }
            if t.starts_with('#') || t.starts_with("//") {
                idx += 1;
                continue;
            }
            body_lines.push(l);
            idx += 1;
        }
        while body_lines.last().is_some_and(|l| l.trim().is_empty()) {
            body_lines.pop();
        }
        let body = if body_lines.is_empty() {
            None
        } else {
            Some(body_lines.join("\n"))
        };

        reqs.push(HttpRequest {
            name: cur_name.take(),
            line: req_line,
            method,
            url,
            headers,
            body,
        });
    }

    (file_vars, reqs)
}

// ---------------------------------------------------------------------------
// Resolução de variáveis {{ ... }}
// ---------------------------------------------------------------------------

fn json_to_plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Navega um JSON por um caminho separado por pontos (ex.: "a.b.c").
fn navigate_json<'a>(mut v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    for part in path.split('.') {
        v = match v {
            Value::Object(m) => m.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

fn resolve_token(
    tok: &str,
    file_vars: &HashMap<String, String>,
    dotenv: &HashMap<String, String>,
    responses: &HashMap<String, StoredResponse>,
) -> Option<String> {
    let tok = tok.trim();

    // {{$dotenv NOME}}
    if let Some(rest) = tok.strip_prefix("$dotenv") {
        return dotenv.get(rest.trim()).cloned();
    }

    // {{nome.response.body.caminho}} e {{nome.response.headers.Header}}
    if let Some((name, rest)) = tok.split_once('.') {
        if let Some(body_path) = rest.strip_prefix("response.body") {
            let path = body_path.trim_start_matches('.');
            let resp = responses.get(name)?;
            return navigate_json(&resp.body, path).map(json_to_plain);
        }
        if let Some(header_path) = rest.strip_prefix("response.headers") {
            let header = header_path.trim_start_matches('.').trim();
            let resp = responses.get(name)?;
            return resp
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(header))
                .map(|(_, v)| v.clone());
        }
        if rest == "response.status" {
            let resp = responses.get(name)?;
            return Some(resp.status.to_string());
        }
        // Outros encadeamentos não suportados.
        return None;
    }

    // {{VAR}} — variável de arquivo (resolvida recursivamente) ou dotenv.
    file_vars
        .get(tok)
        .cloned()
        .or_else(|| dotenv.get(tok).cloned())
}

/// Substitui `{{...}}` recursivamente. Tokens não resolvidos são mantidos.
fn resolve_vars(
    input: &str,
    file_vars: &HashMap<String, String>,
    dotenv: &HashMap<String, String>,
    responses: &HashMap<String, StoredResponse>,
) -> String {
    let mut s = input.to_string();
    for _ in 0..10 {
        let mut out = String::with_capacity(s.len());
        let mut changed = false;
        let mut rest = s.as_str();
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            if let Some(end) = after.find("}}") {
                let tok = &after[..end];
                if let Some(val) = resolve_token(tok, file_vars, dotenv, responses) {
                    out.push_str(&val);
                    changed = true;
                } else {
                    out.push_str("{{");
                    out.push_str(tok);
                    out.push_str("}}");
                }
                rest = &after[end + 2..];
            } else {
                out.push_str("{{");
                rest = after;
            }
        }
        out.push_str(rest);
        s = out;
        if !changed {
            break;
        }
    }
    s
}

/// Coleta tokens `{{...}}` que sobraram sem resolução, para diagnóstico
/// (evita cair no ureq com uma URI inválida e um erro críptico).
fn collect_unresolved(s: &str, out: &mut Vec<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let tok = after[..end].trim().to_string();
        if !tok.is_empty() && !out.contains(&tok) {
            out.push(tok);
        }
        rest = &after[end + 2..];
    }
}

/// Lê um arquivo referenciado por `< caminho`, resolvendo relativo a `base_dir`
/// quando o caminho não é absoluto. Conteúdo não-UTF8 é lido com perda.
///
/// A leitura é confinada ao workspace (`root`), ou à pasta do próprio `.http`
/// quando não há workspace: o caminho é canonicalizado (resolvendo `..` e
/// symlinks) e recusado se escapar do limite. Sem isso, um `.http` malicioso
/// poderia incluir `/etc/passwd` ou `../../.ssh/id_rsa` e enviar o conteúdo
/// para uma URL controlada por ele.
fn read_include(base_dir: Option<&Path>, root: Option<&Path>, path: &str) -> Option<String> {
    let p = Path::new(path);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir?.join(p)
    };
    // Canonicaliza para resolver `..`/symlinks antes de checar o limite; um erro
    // aqui também cobre o caso "arquivo não encontrado".
    let full = match full.canonicalize() {
        Ok(f) => f,
        Err(e) => {
            log(format!("falha ao incluir arquivo {}: {e}", full.display()));
            return None;
        }
    };
    // Limite permitido: a raiz do workspace, ou a pasta do .http se não houver.
    match root.or(base_dir).and_then(|b| b.canonicalize().ok()) {
        Some(b) if full.starts_with(&b) => {}
        Some(b) => {
            log(format!(
                "inclusão bloqueada: {} está fora de {}",
                full.display(),
                b.display()
            ));
            return None;
        }
        None => {
            log(format!(
                "inclusão bloqueada: sem limite de workspace para validar {}",
                full.display()
            ));
            return None;
        }
    }
    match std::fs::read(&full) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            log(format!("falha ao incluir arquivo {}: {e}", full.display()));
            None
        }
    }
}

/// Expande diretivas de inclusão de arquivo no corpo (estilo REST Client):
///   `< caminho`  → insere o conteúdo do arquivo (cru);
///   `<@ caminho` → insere o conteúdo e resolve `{{...}}` dentro dele.
/// Linhas cujo arquivo não pôde ser lido são mantidas como estão.
fn expand_file_includes(
    body: &str,
    base_dir: Option<&Path>,
    root: Option<&Path>,
    file_vars: &HashMap<String, String>,
    dotenv: &HashMap<String, String>,
    responses: &HashMap<String, StoredResponse>,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("<@") {
            match read_include(base_dir, root, rest.trim()) {
                Some(c) => out.push(resolve_vars(&c, file_vars, dotenv, responses)),
                None => out.push(line.to_string()),
            }
        } else if let Some(rest) = t.strip_prefix('<') {
            match read_include(base_dir, root, rest.trim()) {
                Some(c) => out.push(c),
                None => out.push(line.to_string()),
            }
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// Mescla um único arquivo `.env` (KEY=VALUE) no mapa, sem sobrescrever chaves
/// já presentes (o chamador visita os diretórios do mais próximo ao mais
/// distante, então o mais próximo vence).
fn merge_dotenv_file(path: &Path, m: &mut HashMap<String, String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(eq) = line.find('=') {
            let k = line[..eq].trim().to_string();
            let mut v = line[eq + 1..].trim().to_string();
            if v.len() >= 2
                && ((v.starts_with('"') && v.ends_with('"'))
                    || (v.starts_with('\'') && v.ends_with('\'')))
            {
                v = v[1..v.len() - 1].to_string();
            }
            m.entry(k).or_insert(v);
        }
    }
}

/// Carrega as variáveis de `.env` para uma requisição, procurando a partir do
/// diretório do arquivo `.http` e subindo até a raiz do workspace. Os `.env`
/// mais próximos do arquivo têm prioridade. Isso suporta `.env` por ambiente
/// (ex.: `.rest/prd/.env`, `.rest/local/.env`) além do `.env` na raiz.
fn load_dotenv(file_dir: Option<&Path>, root: Option<&str>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let root_path = root.map(Path::new);

    // Diretórios candidatos, do mais próximo (pasta do arquivo) ao mais distante.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur = file_dir;
    while let Some(d) = cur {
        dirs.push(d.to_path_buf());
        if Some(d) == root_path {
            break;
        }
        cur = d.parent();
    }
    // Garante que a raiz seja consultada mesmo se o arquivo estiver fora dela.
    if let Some(r) = root_path {
        if !dirs.iter().any(|d| d.as_path() == r) {
            dirs.push(r.to_path_buf());
        }
    }

    for dir in &dirs {
        merge_dotenv_file(&dir.join(".env"), &mut m);
    }
    m
}

// ---------------------------------------------------------------------------
// Buffer de resultado
// ---------------------------------------------------------------------------

/// Caminho do resultado: um arquivo por workspace, para que dois projetos
/// abertos ao mesmo tempo não sobrescrevam a resposta um do outro. O nome do
/// workspace também identifica a aba, que fica fora do projeto.
fn result_path(root: Option<&str>) -> PathBuf {
    let name = root
        .and_then(|r| Path::new(r).file_name())
        .map(|n| format!("{}.http", n.to_string_lossy()))
        .unwrap_or_else(|| RESULT_FALLBACK.to_string());
    artifact_dir().join(RESULT_DIR).join(name)
}

fn result_uri_for(root: Option<&str>) -> String {
    format!("file://{}", result_path(root).display())
}

/// Garante a pasta de resultados. Chamada a cada escrita, e não uma vez na
/// inicialização, porque `/tmp` é limpo periodicamente em muitas distros e a
/// pasta pode sumir no meio da sessão.
fn ensure_result_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if dir.exists() {
            return;
        }
        if let Err(e) = create_dir_restricted(dir, true) {
            log(format!("falha ao criar {}: {e}", dir.display()));
        }
    }
}

/// Atualiza o resultado escrevendo direto no arquivo. Se o buffer aberto
/// estiver LIMPO (salvo), o watcher do Zed recarrega no lugar — sem
/// `applyEdit`, portanto sem "revelar"/roubar o foco. Depende do buffer estar
/// salvo (via autosave); por isso as atualizações usam este caminho e a
/// abertura inicial usa [`open_result`].
fn write_result(root: Option<&str>, content: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let path = result_path(root);
    ensure_result_dir(&path);
    match open_write_restricted(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                log(format!("falha ao escrever resultado em {}: {e}", path.display()));
            }
        }
        Err(e) => log(format!("falha ao abrir resultado em {}: {e}", path.display())),
    }
}

/// Abre a aba de resultado (quando ainda não está aberta) via `applyEdit`
/// (`CreateFile` novo + edit) — a única forma de o Zed abrir uma aba visível.
/// O buffer nasce "sujo"; o autosave o limpa em seguida, e as atualizações
/// posteriores passam a usar [`write_result`] (disco), sem reveal.
fn open_result(sender: &Sender<Message>, root: Option<&str>, content: &str) {
    let path = result_path(root);
    // Cria um arquivo realmente novo — só assim o Zed abre uma aba visível.
    {
        let _guard = WRITE_LOCK.lock().unwrap();
        ensure_result_dir(&path);
        let _ = std::fs::remove_file(&path);
    }
    apply_result_edit(sender, root, content, true);
}

/// Substitui o conteúdo do buffer de resultado via `applyEdit`, sem criar o
/// arquivo. Ao contrário de [`write_result`], funciona mesmo com o buffer
/// "sujo" (não salvo) — o watcher do Zed ignora mudanças em disco nesse caso.
fn edit_result(sender: &Sender<Message>, root: Option<&str>, content: &str) {
    apply_result_edit(sender, root, content, false);
}

fn apply_result_edit(sender: &Sender<Message>, root: Option<&str>, content: &str, create: bool) {
    let uri_str = result_uri_for(root);
    let Ok(uri) = Uri::from_str(&uri_str) else {
        log(format!("uri de resultado inválida: {uri_str}"));
        return;
    };
    let mut ops = Vec::new();
    if create {
        ops.push(DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
            uri: uri.clone(),
            options: Some(CreateFileOptions {
                overwrite: Some(false),
                ignore_if_exists: Some(false),
            }),
            annotation_id: None,
        })));
    }
    ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
        edits: vec![OneOf::Left(TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(u32::MAX, u32::MAX)),
            new_text: content.to_string(),
        })],
    }));
    let params = ApplyWorkspaceEditParams {
        label: Some("Resposta HTTP".into()),
        edit: WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(ops)),
            change_annotations: None,
        },
    };
    if let Ok(params) = serde_json::to_value(params) {
        let id = RequestId::from(next_n());
        let _ = sender.send(Message::Request(LspRequest {
            id,
            method: "workspace/applyEdit".into(),
            params,
        }));
    }
}

/// Indicador de progresso na barra de status do Zed (`$/progress`).
///
/// É o único indicador que não depende de layout nem de foco. O Code Lens
/// `⏳ Enviando…` não serve para isso: o Zed só desenha o que ele pediu, e ele
/// para de pedir os lenses do `.http` de origem assim que o buffer de
/// resultado vira o editor ativo — o que acontece já na primeira requisição.
struct Progress<'a> {
    sender: &'a Sender<Message>,
    token: String,
}

impl<'a> Progress<'a> {
    fn begin(sender: &'a Sender<Message>, title: impl Into<String>) -> Self {
        let token = format!("http-request-{}", next_n());
        // O Zed descarta $/progress de tokens que não foram registrados por
        // este request, e o registro dele roda numa task separada — daí a
        // pausa antes do "begin".
        let _ = sender.send(Message::Request(LspRequest {
            id: RequestId::from(next_n()),
            method: "window/workDoneProgress/create".into(),
            params: serde_json::json!({ "token": token }),
        }));
        std::thread::sleep(Duration::from_millis(30));
        let _ = sender.send(Message::Notification(Notification {
            method: "$/progress".into(),
            params: serde_json::json!({
                "token": token,
                "value": { "kind": "begin", "title": title.into(), "cancellable": false },
            }),
        }));
        Self { sender, token }
    }
}

impl Drop for Progress<'_> {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Notification(Notification {
            method: "$/progress".into(),
            params: serde_json::json!({ "token": self.token, "value": { "kind": "end" } }),
        }));
    }
}

fn refresh_code_lens(sender: &Sender<Message>) {
    let id = RequestId::from(next_n());
    let _ = sender.send(Message::Request(LspRequest {
        id,
        method: "workspace/codeLens/refresh".into(),
        params: Value::Null,
    }));
}

/// Instantes (desde o `didOpen`) em que os Code Lens são re-pedidos.
///
/// Quem desenha os lenses é o *editor*, e ele só busca os buffers que já estão
/// registrados e visíveis nele. Duas corridas fazem essa busca cair no vazio, e
/// nenhuma delas reagenda nada depois:
///
/// - abrir um segundo `.http`: a busca pode chegar antes do registro do buffer;
/// - abrir o Zed com `.http` já abertos: a restauração do workspace é
///   assíncrona, então a busca pode acontecer antes de o editor existir. O
///   servidor chega a responder os lenses (dá para ver no log) e mesmo assim a
///   aba fica sem botões, até ser fechada e reaberta.
///
/// Os pedidos tardios cobrem as duas. São baratos: cada um só faz o Zed
/// re-perguntar os lenses dos `.http` visíveis.
const LENS_NUDGES_MS: [u64; 4] = [50, 400, 1_500, 4_000];

/// Pede o refresh dos Code Lens algumas vezes depois de um `didOpen`.
fn nudge_code_lens(sender: &Sender<Message>) {
    let sender = sender.clone();
    std::thread::spawn(move || {
        let mut previous = 0;
        for at in LENS_NUDGES_MS {
            std::thread::sleep(Duration::from_millis(at - previous));
            previous = at;
            refresh_code_lens(&sender);
        }
    });
}

fn show_message(sender: &Sender<Message>, typ: MessageType, message: impl Into<String>) {
    let message = message.into();
    if let Ok(params) = serde_json::to_value(ShowMessageParams { typ, message }) {
        let _ = sender.send(Message::Notification(Notification {
            method: "window/showMessage".into(),
            params,
        }));
    }
}

// ---------------------------------------------------------------------------
// Execução da requisição (em thread separada)
// ---------------------------------------------------------------------------

/// Chave do "ambiente" que isola as respostas guardadas: a pasta do arquivo
/// `.http`. Assim `.rest/hml/api.http` e `.rest/prd/api.http` têm tokens
/// independentes, enquanto dois `.http` da mesma pasta compartilham.
///
/// Sem pasta (documento sem caminho em disco), usa a própria uri — pior caso é
/// não compartilhar com ninguém, nunca vazar de um ambiente para outro.
fn response_scope(uri: &str, base_dir: Option<&Path>) -> String {
    base_dir
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| uri.to_string())
}

/// Remove a query string de uma URL para registro/exibição. Os valores em
/// `?a=b` costumam carregar tokens e identificadores, e o log fica legível a
/// outros processos.
fn url_no_query(url: &str) -> &str {
    url.split_once('?').map(|(base, _)| base).unwrap_or(url)
}

fn format_response(
    code: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &str,
    content_type: &str,
) -> String {
    let mut out = format!("HTTP/1.1 {code} {reason}\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push('\n');
    let body_fmt = if content_type.contains("json") {
        serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| body.to_string())
    } else {
        body.to_string()
    };
    out.push_str(&body_fmt);
    out.push('\n');
    out
}

fn perform_request(
    state: &Shared,
    sender: &Sender<Message>,
    uri: String,
    req: HttpRequest,
    file_vars: HashMap<String, String>,
    dotenv: HashMap<String, String>,
    root: Option<String>,
    // Quando o `⏳ Enviando…` foi pedido, para respeitar MIN_LOADING.
    loading_since: Instant,
) {
    // Diretório do arquivo .http. Serve para duas coisas: base dos includes
    // `< caminho/relativo` e chave do "ambiente" que isola as respostas.
    let base_dir = uri
        .strip_prefix("file://")
        .map(Path::new)
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .or_else(|| root.as_deref().map(PathBuf::from));
    let scope = response_scope(&uri, base_dir.as_deref());

    // Snapshot das respostas do MESMO ambiente (para encadeamento), sem segurar
    // o lock. Respostas de outros ambientes ficam invisíveis aqui de propósito.
    let responses = state
        .lock()
        .unwrap()
        .responses
        .get(&scope)
        .cloned()
        .unwrap_or_default();

    let method = resolve_vars(&req.method, &file_vars, &dotenv, &responses);
    let url = resolve_vars(&req.url, &file_vars, &dotenv, &responses);
    let headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| {
            (
                resolve_vars(k, &file_vars, &dotenv, &responses),
                resolve_vars(v, &file_vars, &dotenv, &responses),
            )
        })
        .collect();
    let body = req.body.as_ref().map(|b| {
        let resolved = resolve_vars(b, &file_vars, &dotenv, &responses);
        expand_file_includes(
            &resolved,
            base_dir.as_deref(),
            root.as_deref().map(Path::new),
            &file_vars,
            &dotenv,
            &responses,
        )
    });

    log(format!("=> {method} {}", url_no_query(&url)));

    // Indicador de progresso na barra de status, encerrado no fim desta função
    // (inclusive em caso de erro) pelo Drop.
    let _progress = Progress::begin(sender, format!("Enviando {method} {}", url_no_query(&url)));

    // Feedback imediato no painel de resultado: "Enviando…" no lugar da resposta
    // anterior. É o que dá para garantir — o Code Lens depende de o Zed re-pedir
    // os lenses do .http de origem, coisa que ele deixa de fazer assim que o
    // buffer de resultado vira o editor ativo.
    let result_uri = result_uri_for(root.as_deref());
    // A abertura da aba (`CreateFile`, que apaga e recria o arquivo) acontece no
    // máximo uma vez por sessão. Perguntar a `docs` se o buffer está aberto não
    // serve: o Zed manda `didClose` mesmo com a aba visível (aba de preview, o
    // mesmo arquivo em dois painéis), e aí o clique seguinte repetia o
    // `CreateFile` — e o Zed abria uma segunda aba do mesmo arquivo.
    //
    // Contrapartida assumida: se o usuário fechar a aba de resultado de
    // propósito, as respostas seguintes continuam sendo escritas no arquivo, mas
    // a aba não é reaberta sozinha (basta reabrir o arquivo). É preferível a
    // duplicar abas, e não há como distinguir esse `didClose` do espúrio.
    let needs_open = {
        let mut guard = state.lock().unwrap();
        let needs = !guard.result_opened;
        guard.result_opened = true;
        needs
    };
    let placeholder = format!("# ⏳ Enviando…\n\n{method} {url}\n");
    if needs_open {
        open_result(sender, root.as_deref(), &placeholder);
    } else {
        write_result(root.as_deref(), &placeholder);
    }

    // Se sobrou algum {{...}} sem resolver, aborta com um erro claro em vez de
    // mandar uma URI/headers inválidos ao ureq ("invalid uri character").
    let mut unresolved = Vec::new();
    collect_unresolved(&method, &mut unresolved);
    collect_unresolved(&url, &mut unresolved);
    for (k, v) in &headers {
        collect_unresolved(k, &mut unresolved);
        collect_unresolved(v, &mut unresolved);
    }
    if let Some(b) = &body {
        collect_unresolved(b, &mut unresolved);
    }

    let content = if !unresolved.is_empty() {
        log(format!("variáveis não resolvidas: {unresolved:?}"));
        let list = unresolved
            .iter()
            .map(|t| format!("  - {}{}{}", "{{", t, "}}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "# Erro: variáveis não resolvidas\n\n{method} {url}\n\n\
             As seguintes variáveis não foram encontradas:\n{list}\n\n\
             Verifique se estão definidas no próprio .http (@nome = valor) ou no \
             .env (para {{$dotenv NOME}}). O .env é procurado a partir da pasta \
             do arquivo .http, subindo até a raiz do workspace.\n"
        )
    } else {
        match do_http(&method, &url, &headers, body.as_deref()) {
            Ok((code, reason, resp_headers, resp_body, content_type)) => {
                // Guarda status + body + headers para encadeamento (se tem nome).
                if let Some(name) = &req.name {
                    let body = serde_json::from_str::<Value>(&resp_body)
                        .unwrap_or_else(|_| Value::String(resp_body.clone()));
                    let stored = StoredResponse {
                        status: code,
                        body,
                        headers: resp_headers.clone(),
                    };
                    state
                        .lock()
                        .unwrap()
                        .responses
                        .entry(scope.clone())
                        .or_default()
                        .insert(name.clone(), stored);
                }
                format_response(code, &reason, &resp_headers, &resp_body, &content_type)
            }
            Err(e) => {
                log(format!("erro na requisição: {e}"));
                format!("# Erro ao executar a requisição\n\n{method} {url}\n\n{e}\n")
            }
        }
    };

    // Já estava aberto (e salvo) → escreve em disco, o watcher recarrega sem
    // reveal. Se fomos nós que acabamos de abrir a aba com o "Enviando…", o
    // buffer ainda pode estar sujo e o watcher ignoraria o disco — então a
    // resposta vai por applyEdit, que substitui o conteúdo de qualquer jeito.
    log(format!("resultado needs_open={needs_open} ({result_uri})"));
    if needs_open {
        edit_result(sender, root.as_deref(), &content);
    } else {
        write_result(root.as_deref(), &content);
    }

    // Limpa o loading e atualiza os Code Lens. O resultado já está escrito, então
    // a espera de MIN_LOADING só atrasa o botão voltar ao normal.
    if let Some(rest) = MIN_LOADING.checked_sub(loading_since.elapsed()) {
        std::thread::sleep(rest);
    }
    state.lock().unwrap().inflight.remove(&(uri, req.line));
    refresh_code_lens(sender);
}

/// Faz a requisição HTTP (bloqueante). Retorna (status, reason, headers, body, content-type).
fn do_http(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> anyhow::Result<(u16, String, Vec<(String, String)>, String, String)> {
    use ureq::http::Request;

    let config = ureq::Agent::config_builder()
        .http_status_as_error(false) // queremos ver o corpo mesmo em 4xx/5xx
        .timeout_global(Some(Duration::from_secs(30)))
        // Ao seguir um redirect, nunca reenvia o header `Authorization`: se o
        // destino responder 3xx apontando para outro host, a credencial não vai
        // junto. É o default do ureq, fixado aqui para não depender dele caso
        // mude numa versão futura.
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
        // Limita a cadeia de redirects e, ao estourar o limite, devolve a última
        // resposta (o 3xx) em vez de derrubar a requisição com um erro.
        .max_redirects(10)
        .max_redirects_will_error(false)
        .build();
    let agent: ureq::Agent = config.into();

    let mut builder = Request::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder.body(body.unwrap_or("").to_string())?;

    let mut resp = agent.run(request)?;

    let status = resp.status();
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("").to_string();

    let mut resp_headers = Vec::new();
    let mut content_type = String::new();
    for (name, value) in resp.headers().iter() {
        let n = name.as_str().to_string();
        let v = value.to_str().unwrap_or("").to_string();
        if n.eq_ignore_ascii_case("content-type") {
            content_type = v.clone();
        }
        resp_headers.push((n, v));
    }

    let resp_body = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((code, reason, resp_headers, resp_body, content_type))
}

// ---------------------------------------------------------------------------
// LSP server
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![CMD_SEND.into(), CMD_NOOP.into()],
            work_done_progress_options: Default::default(),
        }),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    };

    let init_params = connection.initialize(serde_json::to_value(&capabilities)?)?;

    let root_path = init_params
        .get("rootPath")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            init_params
                .get("rootUri")
                .and_then(|v| v.as_str())
                .and_then(|u| u.strip_prefix("file://"))
                .map(String::from)
        });
    // Só agora sabemos o workspace, e é ele que dá o nome do arquivo de log.
    init_log(root_path.as_deref());
    log("\n=== http request client lsp starting ===");
    log(format!("root_path = {root_path:?}"));

    let state: Shared = Arc::new(Mutex::new(State {
        root_path,
        ..Default::default()
    }));

    main_loop(&connection, state)?;
    io_threads.join()?;
    log("=== http request client lsp exiting ===");
    Ok(())
}

fn main_loop(connection: &Connection, state: Shared) -> anyhow::Result<()> {
    let result_uri = {
        let guard = state.lock().unwrap();
        result_uri_for(guard.root_path.as_deref())
    };

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                handle_request(connection, &state, &result_uri, req)?;
            }
            Message::Notification(not) => {
                handle_notification(&state, &connection.sender, &result_uri, not);
            }
            Message::Response(resp) => {
                log(format!("<- response (id {:?}): {:?}", resp.id, resp.response_result));
            }
        }
    }
    Ok(())
}

fn handle_notification(
    state: &Shared,
    sender: &Sender<Message>,
    result_uri: &str,
    not: Notification,
) {
    match not.method.as_str() {
        "textDocument/didOpen" => {
            if let Some(td) = not.params.get("textDocument") {
                if let (Some(uri), Some(text)) = (
                    td.get("uri").and_then(|v| v.as_str()),
                    td.get("text").and_then(|v| v.as_str()),
                ) {
                    log(format!("<- didOpen {uri}"));
                    state.lock().unwrap().docs.insert(uri.to_string(), text.to_string());
                    if uri != result_uri {
                        nudge_code_lens(sender);
                    }
                }
            }
        }
        "textDocument/didChange" => {
            let uri = not
                .params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str());
            log(format!("<- didChange {}", uri.unwrap_or("?")));
            // Sync FULL: o texto completo vem na última contentChange.
            let text = not
                .params
                .get("contentChanges")
                .and_then(|v| v.as_array())
                .and_then(|a| a.last())
                .and_then(|c| c.get("text"))
                .and_then(|v| v.as_str());
            if let (Some(uri), Some(text)) = (uri, text) {
                state.lock().unwrap().docs.insert(uri.to_string(), text.to_string());
                // Ao editar um .http de origem, os Code Lens ancorados ficam com
                // a linha defasada. Forçamos o Zed a re-pedir os lenses (com as
                // linhas atuais). Ignora o próprio arquivo de resultado.
                if uri != result_uri {
                    refresh_code_lens(sender);
                }
            }
        }
        "textDocument/didClose" => {
            if let Some(uri) = not.params.pointer("/textDocument/uri").and_then(|v| v.as_str()) {
                log(format!("<- didClose {uri}"));
                state.lock().unwrap().docs.remove(uri);
            }
        }
        _ => {}
    }
}

fn handle_request(
    connection: &Connection,
    state: &Shared,
    result_uri: &str,
    req: LspRequest,
) -> anyhow::Result<()> {
    match req.method.as_str() {
        "textDocument/codeLens" => {
            let params: CodeLensParams = serde_json::from_value(req.params)?;
            let uri = params.text_document.uri.as_str().to_string();
            let lenses = code_lenses(state, result_uri, &uri);
            let sending = lenses
                .iter()
                .filter(|l| l.command.as_ref().is_some_and(|c| c.command == CMD_NOOP))
                .count();
            log(format!("-> codeLens {uri}: {} lens, {sending} enviando", lenses.len()));
            connection.sender.send(Message::Response(Response::new_ok(req.id, lenses)))?;
        }
        "workspace/executeCommand" => {
            let params: ExecuteCommandParams = serde_json::from_value(req.params)?;
            handle_execute_command(connection, state, params);
            connection
                .sender
                .send(Message::Response(Response::new_ok(req.id, Value::Null)))?;
        }
        other => {
            log(format!("-> request não tratado: {other}"));
            connection
                .sender
                .send(Message::Response(Response::new_ok(req.id, Value::Null)))?;
        }
    }
    Ok(())
}

fn code_lenses(state: &Shared, result_uri: &str, uri: &str) -> Vec<CodeLens> {
    // Não mostra "Send request" no próprio buffer de resultado.
    if uri == result_uri {
        return Vec::new();
    }
    let guard = state.lock().unwrap();
    let text = guard.document_text(uri).unwrap_or_default();
    let (_vars, reqs) = parse_document(&text);

    reqs.into_iter()
        .map(|r| {
            let range = Range::new(Position::new(r.line, 0), Position::new(r.line, 0));
            if guard.inflight.contains(&(uri.to_string(), r.line)) {
                CodeLens {
                    range,
                    command: Some(LspCommand::new("⏳ Enviando…".into(), CMD_NOOP.into(), None)),
                    data: None,
                }
            } else {
                let title = match &r.name {
                    Some(n) => format!("▶ Send request  ({n})"),
                    None => "▶ Send request".into(),
                };
                let args = vec![Value::String(uri.to_string()), Value::from(r.line)];
                CodeLens {
                    range,
                    command: Some(LspCommand::new(title, CMD_SEND.into(), Some(args))),
                    data: None,
                }
            }
        })
        .collect()
}

fn handle_execute_command(connection: &Connection, state: &Shared, params: ExecuteCommandParams) {
    if params.command == CMD_NOOP {
        return;
    }
    if params.command != CMD_SEND {
        log(format!("comando desconhecido: {}", params.command));
        return;
    }

    let uri = params.arguments.first().and_then(|v| v.as_str()).map(String::from);
    let line = params.arguments.get(1).and_then(|v| v.as_u64()).map(|n| n as u32);
    let (Some(uri), Some(line)) = (uri, line) else {
        log("sendRequest sem argumentos válidos");
        return;
    };

    // Localiza a requisição e as variáveis de arquivo a partir do texto guardado.
    let (text, root_path) = {
        let guard = state.lock().unwrap();
        (guard.document_text(&uri), guard.root_path.clone())
    };
    let Some(text) = text else {
        log(format!("documento não encontrado: {uri}"));
        return;
    };
    let (file_vars, reqs) = parse_document(&text);
    let Some(req) = reqs.into_iter().find(|r| r.line == line) else {
        log(format!("requisição na linha {line} não encontrada"));
        return;
    };

    // Impede empilhar duas execuções da mesma requisição. A garantia tem que
    // estar aqui, e não na aparência do Code Lens: o Zed só desenha os lenses
    // que ele pede, e ele para de pedir os do `.http` de origem assim que o
    // painel de resultado vira o editor ativo — então o botão pode continuar
    // dizendo "Send request" mesmo com a requisição em andamento.
    // `insert` devolve false quando a chave já estava lá.
    if !state.lock().unwrap().inflight.insert((uri.clone(), line)) {
        let what = req
            .name
            .clone()
            .unwrap_or_else(|| format!("{} {}", req.method, req.url));
        log(format!("clique ignorado, já em andamento: {what} ({uri}:{line})"));
        show_message(
            &connection.sender,
            MessageType::WARNING,
            format!("⏳ {what} já está em andamento — aguarde a resposta."),
        );
        return;
    }

    // Procura o .env a partir da pasta do arquivo .http, subindo até a raiz.
    let file_dir = uri
        .strip_prefix("file://")
        .map(Path::new)
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let dotenv = load_dotenv(file_dir.as_deref(), root_path.as_deref());

    // Pede o ⏳ no lugar do botão (o Zed atende quando está pedindo os lenses).
    let sender = connection.sender.clone();
    refresh_code_lens(&sender);
    let loading_since = Instant::now();

    // Faz a requisição em background para não travar o loop do LSP.
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        perform_request(&state, &sender, uri, req, file_vars, dotenv, root_path, loading_since);
    });
}
