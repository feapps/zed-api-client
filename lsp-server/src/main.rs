//! HTTP Request Client — language server para arquivos `.http` (estilo REST Client).
//!
//! Fornece um Code Lens "Send request" acima de cada requisição; ao clicar,
//! o servidor faz a requisição HTTP de verdade e escreve a resposta formatada
//! num buffer de resultado (caminho fixo) que o Zed abre num painel ao lado.
//!
//! O resultado é escrito em disco num caminho estável por workspace e mostrado
//! com `window/showDocument`, que é idempotente: não duplica aba, não deixa o
//! buffer sujo (então as atualizações chegam pelo watcher, sem roubar foco) e
//! reabre a aba se ela tiver sido fechada. Clientes que não anunciam
//! `window/showDocument` caem no mecanismo anterior — `applyEdit` + `CreateFile`
//! na 1ª vez, substituindo o conteúdo depois —, validado empiricamente no Zed
//! (ver o histórico de spikes / memória do projeto).

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::Local;
use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request as LspRequest, RequestId, Response};
use lsp_types::{
    ApplyWorkspaceEditParams, CodeLens, CodeLensOptions, CodeLensParams, Command as LspCommand,
    CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, ExecuteCommandOptions,
    ExecuteCommandParams, MessageType, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    Range, ResourceOp, ServerCapabilities, ShowDocumentParams, ShowMessageParams, TextDocumentEdit,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Uri, WorkspaceEdit,
};
use serde::{Deserialize, Serialize};
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
/// Arquivo onde as respostas nomeadas são persistidas (ver [`load_responses`]).
const RESPONSES_FILE: &str = "responses.json";
/// Teto por resposta guardada. Acima dele a resposta só vive em memória (dá para
/// encadear na sessão, mas não sobrevive a um reinício do servidor) — o que
/// interessa é o token continuar sendo salvo mesmo com uma listagem enorme ao
/// lado. Ver [`save_responses`].
const MAX_RESPONSE_ENTRY_BYTES: usize = 512 << 10;
/// Teto do arquivo de respostas persistidas inteiro, como último freio.
const MAX_RESPONSES_BYTES: usize = 8 << 20;
/// Teto do arquivo de log: o diretório agora é estável por workspace, então o
/// log sobrevive aos reinícios do servidor e cresceria sem fim.
const MAX_LOG_BYTES: u64 = 2 << 20;
/// Tempo mínimo que o `⏳ Sending…` fica no lugar do Code Lens.
///
/// O Zed espera 50 ms (debounce) + 30 ms antes de pedir os lenses de volta, e
/// cada `workspace/codeLens/refresh` novo *substitui* o pedido pendente em vez
/// de enfileirá-lo. Numa requisição rápida (localhost responde em poucos ms) o
/// refresh do fim cancela o do começo e o indicador nunca chega a ser
/// desenhado. Segurar o estado de loading afasta os dois refreshes o bastante
/// para o Zed renderizar o do meio — e dá tempo de o olho pegar.
const MIN_LOADING: Duration = Duration::from_millis(400);
/// Quanto esperar pela resposta de um `window/showDocument` antes de considerar
/// que o cliente não trata a requisição (ver [`show_result`]). Folgado de
/// propósito: desistir cedo de um cliente que só está ocupado é que abriria a
/// aba duas vezes.
const SHOW_DOCUMENT_TIMEOUT: Duration = Duration::from_millis(3_000);
/// Teto default de duração de uma requisição, quando o `.http` não pede outro.
///
/// Vale para a operação inteira (DNS + conexão + envio + resposta), não por
/// etapa. Pode ser trocado por requisição com `# @timeout <segundos>` ou por
/// workspace com [`TIMEOUT_ENV_KEY`] no `.env`; `0` em qualquer um dos dois
/// remove o limite. Ver [`resolve_timeout`].
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Chave do `.env` que troca o [`DEFAULT_TIMEOUT`] deste workspace, em segundos.
///
/// O nome é longo de propósito: o `.env` consultado é o do projeto, que
/// costuma ser o da própria aplicação, e um `TIMEOUT` solto ali colidiria com
/// a configuração de alguém.
const TIMEOUT_ENV_KEY: &str = "HTTP_REQUEST_TIMEOUT";
/// Quanto esperar depois de um `didClose` antes de apagar as respostas daquele
/// `.http` (ver [`schedule_response_cleanup`]).
///
/// O Zed manda `didClose` espúrio — aba de preview trocada, o mesmo arquivo em
/// dois painéis — normalmente seguido de `didOpen` no mesmo instante. Apagar na
/// hora torraria o token de quem só mudou de aba.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

static COUNTER: AtomicI32 = AtomicI32::new(1);
/// Serializa as escritas nos arquivos de artefato (resultado e respostas).
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Arquivo de log, definido por [`init_log`] a partir da raiz do workspace.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Diretório privado dos artefatos deste workspace em `temp_dir()`:
/// `<temp>/http-request-client-<uid>/<workspace>-<hash>/`. Todos os artefatos
/// (buffer de resultado, respostas persistidas e logs) vivem dentro dele — assim
/// outro usuário da máquina não consegue ler as respostas (que podem conter
/// tokens) nem plantar um symlink para desviar as escritas, já que não consegue
/// nem atravessar o diretório de cima (0700, dono conferido).
///
/// O caminho é **estável**: depende só do usuário e da raiz do workspace, não do
/// processo. Ele já foi aleatório por processo, e isso era um bug — o Zed para o
/// language server quando o último `.http` fecha e sobe outro no próximo que
/// abrir, então cada ciclo estreava um caminho de resultado e o Zed abria mais
/// uma aba de resposta, deixando as antigas órfãs (foi assim que apareceram
/// dezenas de `/tmp/http-request-client-*`).
static ARTIFACT_DIR: OnceLock<PathBuf> = OnceLock::new();

fn next_n() -> i32 {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Toma o lock do estado ignorando envenenamento.
///
/// Um `unwrap()` aqui transformava um pânico em qualquer thread num servidor
/// zumbi: o lock ficava envenenado e todas as threads seguintes morriam ao
/// tentar tomá-lo — o processo continuava vivo, sem responder nada, e a UI ficava
/// travada no `⏳ Sending…`. Um estado eventualmente inconsistente é bem melhor
/// do que isso.
fn lock_state(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_writes() -> MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hash FNV-1a de 64 bits — só para derivar nomes de diretório e comparar
/// assinaturas de Code Lens, nunca para nada com requisito criptográfico.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
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

/// Uid do processo, obtido sem depender de `libc`: cria um arquivo só nosso em
/// `temp_dir()` e lê o dono dele.
#[cfg(unix)]
fn own_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    static UID: OnceLock<Option<u32>> = OnceLock::new();
    *UID.get_or_init(|| {
        let probe = std::env::temp_dir().join(format!(".http-request-client-{}", random_token()));
        let uid = open_write_restricted(&probe)
            .ok()
            .and_then(|_| std::fs::symlink_metadata(&probe).ok())
            .map(|md| md.uid());
        let _ = std::fs::remove_file(&probe);
        uid
    })
}

/// Confere que o caminho é um diretório nosso, com permissão 0700 e que *não* é
/// um symlink — a checagem que impede reusar um diretório plantado por outro
/// usuário no `/tmp` compartilhado.
fn is_private_dir(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !md.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        if md.permissions().mode() & 0o777 != 0o700 {
            return false;
        }
        if Some(md.uid()) != own_uid() {
            return false;
        }
    }
    true
}

/// Diretório base dos artefatos, um por usuário: `<temp>/http-request-client-<uid>`.
/// Devolve `None` (e o chamador cai no diretório aleatório) se o caminho existir
/// e não passar em [`is_private_dir`].
fn stable_base_dir() -> Option<PathBuf> {
    // No Windows o `temp_dir()` já é por usuário; no Unix o uid vai no nome para
    // dois usuários não disputarem o mesmo caminho em `/tmp`.
    #[cfg(unix)]
    let tag = own_uid()?.to_string();
    #[cfg(not(unix))]
    let tag = "user".to_string();

    let base = std::env::temp_dir().join(format!("http-request-client-{tag}"));
    match create_dir_restricted(&base, false) {
        Ok(()) => Some(base),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if is_private_dir(&base) {
                Some(base)
            } else {
                eprintln!(
                    "http-request-client-lsp: {} existe e não é um diretório privado nosso; \
                     usando um diretório aleatório",
                    base.display()
                );
                None
            }
        }
        Err(_) => None,
    }
}

/// Nome do subdiretório do workspace: nome legível + hash do caminho completo
/// (dois projetos podem ter o mesmo nome de pasta em raízes diferentes).
fn workspace_key(root: Option<&str>) -> String {
    let Some(root) = root else {
        return "no-workspace".to_string();
    };
    let name: String = Path::new(root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let name = if name.is_empty() { "workspace" } else { name.as_str() };
    format!("{name}-{:016x}", fnv1a(root.as_bytes()))
}

/// Diretório aleatório de fallback, quando o estável não está disponível. Perde
/// a continuidade entre reinícios, mas nunca a privacidade.
fn random_artifact_dir() -> PathBuf {
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
}

/// Fixa o diretório de artefatos deste workspace. Chamado uma vez, logo depois
/// do `initialize` (é de lá que vem a raiz).
fn init_artifacts(root: Option<&str>) {
    let dir = stable_base_dir()
        .map(|base| base.join(workspace_key(root)))
        .filter(|dir| match create_dir_restricted(dir, true) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "http-request-client-lsp: não consegui criar {} ({e}); usando um \
                     diretório aleatório",
                    dir.display()
                );
                false
            }
        })
        .unwrap_or_else(random_artifact_dir);
    let _ = ARTIFACT_DIR.set(dir);
}

/// Diretório dos artefatos (ver [`ARTIFACT_DIR`]). Quem chamar antes de
/// [`init_artifacts`] recebe um diretório aleatório.
fn artifact_dir() -> &'static Path {
    ARTIFACT_DIR.get_or_init(random_artifact_dir).as_path()
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
    let path = artifact_dir().join(name);
    // O diretório é estável, então o log de ontem ainda está aqui: zera quando
    // passa do teto em vez de crescer para sempre.
    if std::fs::metadata(&path).is_ok_and(|md| md.len() > MAX_LOG_BYTES) {
        let _ = open_write_restricted(&path);
    }
    let _ = LOG_PATH.set(path);
}

/// Horário local no formato do `Zed.log` (RFC 3339 com offset), mais os
/// milissegundos.
///
/// O mesmo formato dos dois lados é o que permite abrir este log e o do Zed lado
/// a lado e casar "cliquei aqui" com "o servidor fez aquilo".
fn now_stamp() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
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
    // Monta a linha inteira antes de escrever. Um `writeln!` com argumentos de
    // formatação emite uma chamada de `write` por pedaço, e como cada requisição
    // roda na sua própria thread as linhas saíam entrelaçadas no arquivo
    // (`=> GET /x<- response (id ...)`) — justamente nos trechos concorrentes que
    // mais interessam ao investigar. Uma escrita só em `O_APPEND` é atômica.
    let line = format!("{} {}\n", now_stamp(), msg.as_ref());
    if let Ok(mut f) = o.open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Estado compartilhado
// ---------------------------------------------------------------------------

/// Resposta guardada de uma requisição nomeada, para encadeamento
/// `{{nome.response.body.campo}}`, `{{nome.response.headers.Header}}` e
/// `{{nome.response.status}}`.
#[derive(Clone, Default, Serialize, Deserialize)]
struct StoredResponse {
    /// Código de status HTTP (ex.: 200).
    status: u16,
    /// Corpo parseado como JSON (ou `Value::String` cru, se não for JSON).
    body: Value,
    /// Headers da resposta, na ordem/caixa originais (lookup é case-insensitive).
    headers: Vec<(String, String)>,
}

/// Respostas nomeadas, por ambiente (ver [`State::responses`]).
type Responses = HashMap<String, HashMap<String, StoredResponse>>;

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
    ///
    /// Persistidas em disco (ver [`save_responses`]): o Zed derruba o language
    /// server quando o último `.http` fecha, e sem isso o token do `# @name
    /// oauthLogin` morria junto — as requisições seguintes falhavam com
    /// "unresolved variables" sem nada na tela explicando por quê.
    responses: Responses,
    /// Requisições em andamento (uri, linha) — para o indicador de loading.
    inflight: HashSet<(String, u32)>,
    /// Raiz do workspace, para localizar o `.env`.
    root_path: Option<String>,
    /// O que o cliente anunciou em `capabilities.window.showDocument.support`.
    /// `None` = não falou nada, e aí tentamos de qualquer forma: o Zed é o
    /// cliente-alvo e pode tratar a requisição sem anunciá-la. Quem diz `false`
    /// explicitamente é levado a sério e vai direto para o fallback.
    show_document_support: Option<bool>,
    /// Ligado quando um `window/showDocument` volta com erro, volta com
    /// `success: false` ou simplesmente não volta ([`SHOW_DOCUMENT_TIMEOUT`]):
    /// daí em diante a aba é aberta pelo caminho antigo (`applyEdit` +
    /// `CreateFile`).
    show_document_failed: bool,
    /// Ids de `window/showDocument` esperando resposta.
    pending_show: HashSet<RequestId>,
    /// Se o cliente tem o buffer de resultado aberto (didOpen/didClose).
    ///
    /// Decide se a resposta precisa ser mostrada — por `window/showDocument`,
    /// que é idempotente (pedir para um arquivo já aberto no máximo revela a aba
    /// existente), ou pelo `CreateFile` do fallback, que não é: por isso, nesse
    /// caminho, o campo também é ligado ao pedirmos a abertura, sem esperar o
    /// `didOpen`, para dois cliques seguidos não duplicarem a aba.
    result_open: bool,
    /// Se o buffer de resultado pode estar "sujo" porque acabamos de criá-lo por
    /// `applyEdit` — nesse estado o watcher ignora o disco e a atualização
    /// precisa ir por `applyEdit` também.
    result_dirty: bool,
    /// Assinatura dos Code Lens por documento (ver [`lens_signature`]), para não
    /// pedir refresh a cada tecla digitada.
    lens_signatures: HashMap<String, u64>,
    /// `.http` que receberam `didClose` e estão no período de carência antes de
    /// ter as respostas apagadas (ver [`schedule_response_cleanup`]).
    pending_clear: HashSet<String>,
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

    /// Se a aba de resultado deve ser aberta por `window/showDocument`.
    fn use_show_document(&self) -> bool {
        !self.show_document_failed && self.show_document_support != Some(false)
    }
}

/// Caminho das respostas persistidas, dentro do diretório do workspace.
fn responses_path() -> PathBuf {
    artifact_dir().join(RESPONSES_FILE)
}

/// Lê as respostas nomeadas da sessão anterior neste mesmo workspace.
///
/// O conteúdo pode incluir tokens de autenticação. Ele já ficava em disco antes
/// (o buffer de resultado é um arquivo), e no mesmo diretório 0700 de dono
/// conferido — não abre uma exposição nova, mas é o motivo de o diretório ser
/// privado de verdade.
fn load_responses() -> Responses {
    let path = responses_path();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                log(format!("ignoring {} (could not be read): {e}", path.display()));
                Responses::default()
            }
        },
        Err(_) => Responses::default(),
    }
}

/// Agenda a limpeza das respostas de um `.http` fechado.
///
/// A carência de [`CLOSE_GRACE`] filtra o `didClose` espúrio: se o documento
/// voltar (`didOpen`) ou der sinal de vida (um pedido de Code Lens) antes de o
/// prazo acabar, a limpeza é cancelada.
fn schedule_response_cleanup(state: &Shared, uri: String) {
    lock_state(state).pending_clear.insert(uri.clone());
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        std::thread::sleep(CLOSE_GRACE);
        let confirmed = {
            let mut guard = lock_state(&state);
            guard.pending_clear.remove(&uri) && !guard.docs.contains_key(&uri)
        };
        if confirmed {
            clear_responses_for(&state, &uri);
        }
    });
}

/// Apaga (memória e disco) as respostas do ambiente de `uri`.
///
/// Como o ambiente é a *pasta* do `.http`, e arquivos da mesma pasta encadeiam
/// entre si, um ambiente só é limpo quando nenhum outro `.http` aberto o
/// compartilha — fechar `login.http` não pode derrubar a sessão do
/// `pedidos.http` que continua aberto ao lado.
fn clear_responses_for(state: &Shared, uri: &str) {
    let snapshot = {
        let mut guard = lock_state(state);
        let root = guard.root_path.clone();
        let scope = doc_scope(uri, root.as_deref());
        let shared_with = guard
            .docs
            .keys()
            .find(|other| *other != uri && doc_scope(other, root.as_deref()) == scope)
            .cloned();
        if let Some(other) = shared_with {
            log(format!("responses for {scope} kept: {other} is still open"));
            return;
        }
        if guard.responses.remove(&scope).is_none() {
            return;
        }
        log(format!("responses for {scope} cleared ({uri} closed)"));
        guard.responses.clone()
    };
    save_responses(&snapshot);
}

/// Executa as limpezas pendentes na hora, sem esperar a carência. Serve para o
/// encerramento: o processo não vai viver até o prazo acabar.
///
/// Só as pendentes, de propósito. O Zed também para e sobe o servidor **sem**
/// fechar arquivo nenhum (dá para ver no log: um `=== starting ===` sem nenhum
/// `didClose` antes); nesses reinícios não há nada pendente e as respostas dos
/// arquivos que seguem abertos ficam onde estão.
fn flush_pending_clears(state: &Shared) {
    let pending: Vec<String> = lock_state(state).pending_clear.drain().collect();
    for uri in pending {
        clear_responses_for(state, &uri);
    }
}

fn save_responses(responses: &Responses) {
    let path = responses_path();

    // Nada guardado (o último `.http` do ambiente fechou): não deixa arquivo para
    // trás.
    if responses.values().all(|m| m.is_empty()) {
        let _guard = lock_writes();
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                log(format!("failed to remove {}: {e}", path.display()));
            }
        }
        return;
    }

    // Respostas gigantes (uma listagem com `page_size=100000`, por exemplo) ficam
    // de fora **uma por uma**, em vez de derrubar a persistência inteira: era o
    // que acontecia com um teto só no total, e o token — 700 bytes — deixava de
    // ser salvo por causa de uma listagem de 2 MB ao lado.
    let mut trimmed = Responses::new();
    let mut skipped: Vec<String> = Vec::new();
    for (scope, named) in responses {
        for (name, resp) in named {
            let size = serde_json::to_vec(resp).map(|v| v.len()).unwrap_or(usize::MAX);
            if size > MAX_RESPONSE_ENTRY_BYTES {
                skipped.push(format!("{name} ({} KiB)", size / 1024));
                continue;
            }
            trimmed.entry(scope.clone()).or_default().insert(name.clone(), resp.clone());
        }
    }
    if !skipped.is_empty() {
        log(format!(
            "large responses kept in memory only: {}",
            skipped.join(", ")
        ));
    }

    let Ok(json) = serde_json::to_vec(&trimmed) else {
        return;
    };
    if json.len() > MAX_RESPONSES_BYTES {
        log(format!(
            "responses not persisted: {} bytes exceed the cap of {MAX_RESPONSES_BYTES}",
            json.len()
        ));
        return;
    }
    let _guard = lock_writes();
    match open_write_restricted(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&json) {
                log(format!("failed to write {}: {e}", path.display()));
            }
        }
        Err(e) => log(format!("failed to open {}: {e}", path.display())),
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
    /// Valor cru de `# @timeout <segundos>`, se a requisição tiver a diretiva.
    ///
    /// Fica cru de propósito: `parse_document` roda a cada `codeLens`, ou seja a
    /// cada tecla digitada no arquivo, então validar aqui significa logar o mesmo
    /// erro dezenas de vezes. Quem interpreta é [`resolve_timeout`], uma vez por
    /// envio. `None` é "não pedi nada, use o default do workspace"; `"0"` é
    /// "sem limite".
    timeout_raw: Option<String>,
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
    let mut cur_timeout: Option<String> = None;
    let mut idx = 0usize;

    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim_start();

        // Separador de requisições.
        if trimmed.starts_with("###") {
            cur_name = None;
            cur_timeout = None;
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

        // Comentário (# ou //). `# @name X` define o nome da próxima requisição
        // e `# @timeout N` o teto dela em segundos.
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            let content = trimmed.trim_start_matches(['#', '/']).trim();
            if let Some(n) = content.strip_prefix("@name") {
                cur_name = Some(n.trim().to_string());
            } else if let Some(t) = content.strip_prefix("@timeout") {
                cur_timeout = Some(t.trim().to_string());
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
            timeout_raw: cur_timeout.take(),
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
            log(format!("failed to include file {}: {e}", full.display()));
            return None;
        }
    };
    // Limite permitido: a raiz do workspace, ou a pasta do .http se não houver.
    match root.or(base_dir).and_then(|b| b.canonicalize().ok()) {
        Some(b) if full.starts_with(&b) => {}
        Some(b) => {
            log(format!(
                "include blocked: {} is outside {}",
                full.display(),
                b.display()
            ));
            return None;
        }
        None => {
            log(format!(
                "include blocked: no workspace boundary to validate {}",
                full.display()
            ));
            return None;
        }
    }
    match std::fs::read(&full) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            log(format!("failed to include file {}: {e}", full.display()));
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
            log(format!("failed to create {}: {e}", dir.display()));
        }
    }
}

/// Atualiza o resultado escrevendo direto no arquivo. Se o buffer aberto
/// estiver LIMPO (salvo), o watcher do Zed recarrega no lugar — sem
/// `applyEdit`, portanto sem "revelar"/roubar o foco. Depende do buffer estar
/// salvo (via autosave); por isso as atualizações usam este caminho e a
/// abertura inicial usa [`open_result`].
fn write_result(root: Option<&str>, content: &str) {
    let _guard = lock_writes();
    let path = result_path(root);
    ensure_result_dir(&path);
    match open_write_restricted(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                log(format!("failed to write result to {}: {e}", path.display()));
            }
        }
        Err(e) => log(format!("failed to open result at {}: {e}", path.display())),
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
        let _guard = lock_writes();
        ensure_result_dir(&path);
        let _ = std::fs::remove_file(&path);
    }
    apply_result_edit(sender, root, content, true);
}

/// Escreve o resultado e garante que ele esteja visível.
///
/// Em qualquer um dos dois caminhos, uma aba fechada é reaberta: depois de um
/// `didClose`, as respostas iriam para um arquivo que ninguém está vendo e o
/// clique em "Send request" pareceria não fazer nada.
///
/// Caminho preferido: escreve em disco e, se o cliente não tem o buffer aberto,
/// pede um `window/showDocument`. É idempotente (não duplica aba) e não deixa o
/// buffer sujo — então as atualizações seguintes chegam pelo watcher, sem roubar
/// o foco.
///
/// Sem `window/showDocument` no cliente (o Zed responde `-32601` a ele), cai no
/// caminho antigo: `applyEdit` + `CreateFile` para abrir, disco para atualizar.
fn publish_result(state: &Shared, sender: &Sender<Message>, root: Option<&str>, content: &str) {
    let (use_show, result_open) = {
        let guard = lock_state(state);
        (guard.use_show_document(), guard.result_open)
    };

    if use_show {
        write_result(root, content);
        if !result_open {
            show_result(state, sender, root);
        }
        return;
    }

    // Sem `window/showDocument`, o `CreateFile` é a única forma de abrir a aba —
    // e ele vale tanto na primeira requisição quanto depois de o usuário fechar
    // a aba. Enquanto isto era feito uma vez por sessão, um `didClose` mandava
    // todas as respostas seguintes para um arquivo que ninguém estava vendo, e o
    // clique em "Send request" parecia não fazer nada.
    let needs_open = {
        let mut guard = lock_state(state);
        let needs = !guard.result_open;
        if needs {
            guard.result_open = true;
            guard.result_dirty = true;
        }
        needs
    };
    if needs_open {
        open_result(sender, root, content);
        return;
    }
    // Recém-criado por `applyEdit`, o buffer pode estar sujo e o watcher ignora
    // o disco; a partir da primeira substituição o autosave já o limpou.
    let was_dirty = std::mem::take(&mut lock_state(state).result_dirty);
    if was_dirty {
        edit_result(sender, root, content);
    } else {
        write_result(root, content);
    }
}

/// Pede ao cliente para mostrar a aba de resultado sem roubar o foco.
///
/// Um cliente que não trate `window/showDocument` deveria responder com erro,
/// mas nem todos respondem — daí o vigia com [`SHOW_DOCUMENT_TIMEOUT`]. Sem ele,
/// um cliente calado deixaria a resposta num arquivo que ninguém está vendo, que
/// é exatamente o sintoma que este caminho existe para eliminar.
fn show_result(state: &Shared, sender: &Sender<Message>, root: Option<&str>) {
    let uri_str = result_uri_for(root);
    let Ok(uri) = Uri::from_str(&uri_str) else {
        log(format!("invalid result uri: {uri_str}"));
        return;
    };
    let params = ShowDocumentParams {
        uri,
        external: Some(false),
        take_focus: Some(false),
        selection: None,
    };
    let Ok(params) = serde_json::to_value(params) else {
        return;
    };
    let id = RequestId::from(next_n());
    lock_state(state).pending_show.insert(id.clone());
    let _ = sender.send(Message::Request(LspRequest {
        id: id.clone(),
        method: "window/showDocument".into(),
        params,
    }));

    let state = Arc::clone(state);
    let sender = sender.clone();
    let root = root.map(String::from);
    std::thread::spawn(move || {
        std::thread::sleep(SHOW_DOCUMENT_TIMEOUT);
        let give_up = {
            let mut guard = lock_state(&state);
            // Se a aba abriu (o cliente mandou o didOpen dela), a requisição
            // funcionou mesmo sem resposta — cair no `CreateFile` aqui é que
            // duplicaria a aba.
            guard.pending_show.remove(&id) && !guard.result_open
        };
        if give_up {
            log("window/showDocument got no answer; using applyEdit + CreateFile");
            fall_back_to_apply_edit(&state, &sender, root.as_deref());
        }
    });
}

/// Desiste do `window/showDocument` e entrega o resultado pelo caminho antigo.
fn fall_back_to_apply_edit(state: &Shared, sender: &Sender<Message>, root: Option<&str>) {
    lock_state(state).show_document_failed = true;
    // O conteúdo já está em disco; reabre por lá para o usuário não ficar sem a
    // resposta desta requisição.
    let content = std::fs::read_to_string(result_path(root)).unwrap_or_default();
    publish_result(state, sender, root, &content);
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
        log(format!("invalid result uri: {uri_str}"));
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
        label: Some("HTTP response".into()),
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
/// `⏳ Sending…` não serve para isso: o Zed só desenha o que ele pediu, e ele
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

/// Assinatura dos Code Lens de um documento: linha e nome de cada requisição.
///
/// Serve para saber se uma edição realmente mexeu nos lenses (só então vale
/// pedir refresh ao cliente) — ver `textDocument/didChange`.
fn lens_signature(text: &str) -> u64 {
    let (_vars, reqs) = parse_document(text);
    let mut shape = String::new();
    for r in &reqs {
        shape.push_str(&r.line.to_string());
        shape.push(':');
        shape.push_str(r.name.as_deref().unwrap_or(""));
        shape.push('\n');
    }
    fnv1a(shape.as_bytes())
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

/// Pasta do arquivo `.http` (base dos includes `< relativo` e do ambiente das
/// respostas). Sem caminho em disco, recorre à raiz do workspace.
fn doc_base_dir(uri: &str, root: Option<&str>) -> Option<PathBuf> {
    uri.strip_prefix("file://")
        .map(Path::new)
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .or_else(|| root.map(PathBuf::from))
}

/// Ambiente de um documento — a mesma chave usada para guardar e para apagar as
/// respostas, por isso mora numa função só.
fn doc_scope(uri: &str, root: Option<&str>) -> String {
    response_scope(uri, doc_base_dir(uri, root).as_deref())
}

/// Identidade de uma requisição, estável a mudanças de posição: método, url e
/// nome. Vai no Code Lens junto com a linha, e é ela que resolve o clique quando
/// a linha do lens está velha.
fn request_key(req: &HttpRequest) -> u64 {
    let mut s = String::new();
    s.push_str(&req.method.to_ascii_uppercase());
    s.push('\n');
    s.push_str(req.url.trim());
    s.push('\n');
    s.push_str(req.name.as_deref().unwrap_or(""));
    fnv1a(s.as_bytes())
}

/// `(chave, n-ésima ocorrência da chave)` de cada requisição do documento. O
/// contador desempata requisições idênticas repetidas no mesmo arquivo.
fn request_keys(reqs: &[HttpRequest]) -> Vec<(u64, u32)> {
    let mut seen: HashMap<u64, u32> = HashMap::new();
    reqs.iter()
        .map(|r| {
            let key = request_key(r);
            let counter = seen.entry(key).or_insert(0);
            let nth = *counter;
            *counter += 1;
            (key, nth)
        })
        .collect()
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

/// Mensagem de erro da requisição, com um texto à parte para timeout.
///
/// O timeout merece o tratamento separado porque a leitura intuitiva dele é
/// errada e custa tempo de investigação: desistir de esperar **não cancela** o
/// trabalho do outro lado. Quem vê o `⏳` sumir com "timeout" clica de novo, e
/// esse segundo clique não substitui a execução anterior — soma outra em cima
/// dela, ainda em andamento no servidor. O sintoma (todas as tentativas
/// seguintes estourando também, mesmo pedindo menos dados) é indistinguível de
/// uma conexão presa no cliente, que é justamente o que **não** está
/// acontecendo: cada requisição abre um `ureq::Agent` novo, com pool e conexão
/// próprios.
fn format_request_error(
    e: &anyhow::Error,
    method: &str,
    url: &str,
    timeout: Option<Duration>,
    timeout_from: &str,
    elapsed: Duration,
) -> String {
    let timed_out = matches!(e.downcast_ref::<ureq::Error>(), Some(ureq::Error::Timeout(_)));
    if !timed_out {
        return format!("# Error running the request\n\n{method} {url}\n\n{e}\n");
    }
    let limit = match timeout {
        Some(d) => format!("{}s", d.as_secs()),
        None => "none".to_string(),
    };
    let mut out = format!("# Timeout after {:.1}s\n\n{method} {url}\n\n", elapsed.as_secs_f64());
    out.push_str(&format!("Limit: {limit} (set by {timeout_from})\n\n"));
    out.push_str(
        "The client stopped waiting, but the server may still be working on this \
         request — a timeout here does not cancel anything on the other side.\n\n\
         Clicking \"Send request\" again does not replace that work: it starts \
         another request on top of the one still running, which usually makes \
         both slower. Prefer waiting, or narrowing the request.\n\n",
    );
    out.push_str("To allow more time:\n\n");
    out.push_str("  - this request only:  # @timeout 120   (seconds, on a line above it)\n");
    out.push_str("  - whole workspace:    HTTP_REQUEST_TIMEOUT=120   (in .env)\n\n");
    out.push_str("Use 0 in either place to wait with no limit.\n");
    out
}

/// Mantém a entrada de `inflight` viva enquanto a requisição roda e a remove no
/// fim — inclusive se a thread entrar em pânico, que antes deixava o Code Lens
/// preso em `⏳ Sending…` para sempre e todo clique seguinte era recusado com
/// "is already running".
struct Inflight<'a> {
    state: &'a Shared,
    sender: &'a Sender<Message>,
    key: (String, u32),
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        lock_state(self.state).inflight.remove(&self.key);
        // O ⏳ só volta a ser "Send request" quando o Zed re-pede os lenses.
        refresh_code_lens(self.sender);
    }
}

fn perform_request(
    state: &Shared,
    sender: &Sender<Message>,
    uri: String,
    req: HttpRequest,
    file_vars: HashMap<String, String>,
    dotenv: HashMap<String, String>,
    root: Option<String>,
    // Quando o `⏳ Sending…` foi pedido, para respeitar MIN_LOADING.
    loading_since: Instant,
) {
    let _inflight = Inflight {
        state,
        sender,
        key: (uri.clone(), req.line),
    };

    // Diretório do arquivo .http. Serve para duas coisas: base dos includes
    // `< caminho/relativo` e chave do "ambiente" que isola as respostas.
    let base_dir = doc_base_dir(&uri, root.as_deref());
    let scope = response_scope(&uri, base_dir.as_deref());

    // Snapshot das respostas do MESMO ambiente (para encadeamento), sem segurar
    // o lock. Respostas de outros ambientes ficam invisíveis aqui de propósito.
    let responses = lock_state(state).responses.get(&scope).cloned().unwrap_or_default();

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

    let (timeout, timeout_from) = resolve_timeout(&req, &dotenv);
    log(format!(
        "=> {method} {} (timeout {}, from {timeout_from})",
        url_no_query(&url),
        match timeout {
            Some(d) => format!("{}s", d.as_secs()),
            None => "no limit".to_string(),
        }
    ));

    // Indicador de progresso na barra de status, encerrado no fim desta função
    // (inclusive em caso de erro) pelo Drop.
    let _progress = Progress::begin(sender, format!("Sending {method} {}", url_no_query(&url)));

    // Feedback imediato no painel de resultado: "Sending…" no lugar da resposta
    // anterior. É o que dá para garantir — o Code Lens depende de o Zed re-pedir
    // os lenses do .http de origem, coisa que ele deixa de fazer assim que o
    // buffer de resultado vira o editor ativo.
    let placeholder = format!("# ⏳ Sending…\n\n{method} {url}\n");
    publish_result(state, sender, root.as_deref(), &placeholder);

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
        log(format!("unresolved variables: {unresolved:?}"));
        let list = unresolved
            .iter()
            .map(|t| format!("  - {}{}{}", "{{", t, "}}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "# Error: unresolved variables\n\n{method} {url}\n\n\
             The following variables were not found:\n{list}\n\n\
             Make sure they are defined in the .http file itself (@name = value) \
             or in .env (for {{$dotenv NAME}}). The .env file is looked up \
             starting from the .http file's folder, walking up to the workspace \
             root.\n"
        )
    } else {
        let started = Instant::now();
        match do_http(&method, &url, &headers, body.as_deref(), timeout) {
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
                    // Persistidas fora do lock: o servidor morre a cada vez que o
                    // último `.http` fecha, e o token precisa sobreviver a isso.
                    let snapshot = {
                        let mut guard = lock_state(state);
                        guard
                            .responses
                            .entry(scope.clone())
                            .or_default()
                            .insert(name.clone(), stored);
                        guard.responses.clone()
                    };
                    save_responses(&snapshot);
                }
                format_response(code, &reason, &resp_headers, &resp_body, &content_type)
            }
            Err(e) => {
                let elapsed = started.elapsed();
                log(format!(
                    "request error after {:.1}s: {e}",
                    elapsed.as_secs_f64()
                ));
                format_request_error(&e, &method, &url, timeout, &timeout_from, elapsed)
            }
        }
    };

    log(format!("result at {}", result_uri_for(root.as_deref())));
    publish_result(state, sender, root.as_deref(), &content);

    // Segura o `⏳` o mínimo combinado; o resultado já está escrito, então a
    // espera só atrasa o botão voltar ao normal. A limpeza do inflight e o
    // refresh dos lenses vêm depois, no Drop de `_inflight`.
    if let Some(rest) = MIN_LOADING.checked_sub(loading_since.elapsed()) {
        std::thread::sleep(rest);
    }
}

/// `0` segundos quer dizer "sem limite", mesma convenção do
/// `rest-client.timeoutinmilliseconds` do REST Client.
fn to_timeout(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Teto desta requisição e de onde ele veio, na ordem: `# @timeout` no próprio
/// `.http`, senão [`TIMEOUT_ENV_KEY`] no `.env`, senão [`DEFAULT_TIMEOUT`].
///
/// A origem volta junto para as mensagens poderem dizer qual das três configurou
/// o valor — sem isso, "timeout" não distingue "o default te pegou" de "o número
/// que você escolheu não foi suficiente".
fn resolve_timeout(
    req: &HttpRequest,
    dotenv: &HashMap<String, String>,
) -> (Option<Duration>, String) {
    // Um valor inválido é ignorado (cai no próximo da ordem) em vez de virar
    // erro: a alternativa seria recusar a requisição por causa de um comentário
    // malformado, com o botão de enviar ali do lado.
    if let Some(raw) = &req.timeout_raw {
        match raw.parse::<u64>() {
            Ok(secs) => return (to_timeout(secs), "# @timeout".to_string()),
            Err(_) => log(format!("invalid @timeout, ignoring: {raw:?}")),
        }
    }
    if let Some(raw) = dotenv.get(TIMEOUT_ENV_KEY) {
        match raw.trim().parse::<u64>() {
            Ok(secs) => return (to_timeout(secs), format!("{TIMEOUT_ENV_KEY} (.env)")),
            Err(_) => log(format!(
                "invalid {TIMEOUT_ENV_KEY} in .env, ignoring: {:?}",
                raw.trim()
            )),
        }
    }
    (Some(DEFAULT_TIMEOUT), "default".to_string())
}

/// Faz a requisição HTTP (bloqueante). Retorna (status, reason, headers, body, content-type).
fn do_http(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout: Option<Duration>,
) -> anyhow::Result<(u16, String, Vec<(String, String)>, String, String)> {
    use ureq::http::Request;

    let config = ureq::Agent::config_builder()
        .http_status_as_error(false) // queremos ver o corpo mesmo em 4xx/5xx
        .timeout_global(timeout)
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
    // Só agora sabemos o workspace, e é ele que define o diretório dos artefatos
    // (e daí o arquivo de log e o de respostas).
    init_artifacts(root_path.as_deref());
    init_log(root_path.as_deref());
    log("\n=== http request client lsp starting ===");
    log(format!("root_path = {root_path:?}"));
    log(format!("artifacts at {}", artifact_dir().display()));

    let show_document_support = init_params
        .pointer("/capabilities/window/showDocument/support")
        .and_then(|v| v.as_bool());
    log(format!("capability window/showDocument: {show_document_support:?}"));

    // Respostas nomeadas da sessão anterior neste workspace, para o
    // encadeamento sobreviver ao ciclo de parada/subida do servidor.
    let responses = load_responses();
    if !responses.is_empty() {
        let total: usize = responses.values().map(|m| m.len()).sum();
        log(format!(
            "{total} response(s) restored from {}",
            responses_path().display()
        ));
    }

    let state: Shared = Arc::new(Mutex::new(State {
        root_path,
        show_document_support,
        responses,
        ..Default::default()
    }));

    main_loop(&connection, state)?;
    io_threads.join()?;
    log("=== http request client lsp exiting ===");
    Ok(())
}

fn main_loop(connection: &Connection, state: Shared) -> anyhow::Result<()> {
    let (result_uri, root_path) = {
        let guard = lock_state(&state);
        (result_uri_for(guard.root_path.as_deref()), guard.root_path.clone())
    };

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    // O processo não vai viver até o fim da carência dos
                    // `didClose` que chegaram agora (fechar o último `.http` faz
                    // o Zed derrubar o servidor).
                    flush_pending_clears(&state);
                    return Ok(());
                }
                handle_request(connection, &state, &result_uri, req)?;
            }
            Message::Notification(not) => {
                handle_notification(&state, &connection.sender, &result_uri, not);
            }
            Message::Response(resp) => {
                log(format!("<- response (id {:?}): {:?}", resp.id, resp.response_result));
                handle_response(&state, &connection.sender, root_path.as_deref(), resp);
            }
        }
    }
    // Cliente fechou o pipe sem `shutdown` — mesma história do branch acima.
    flush_pending_clears(&state);
    Ok(())
}

/// Trata as respostas às requisições que *nós* fizemos. Só `window/showDocument`
/// interessa: se o cliente recusar, a aba passa a ser aberta pelo caminho antigo.
fn handle_response(
    state: &Shared,
    sender: &Sender<Message>,
    root: Option<&str>,
    resp: Response,
) {
    let was_show = lock_state(state).pending_show.remove(&resp.id);
    if !was_show {
        return;
    }
    let refused = match &resp.response_result {
        Ok(value) => value.get("success").and_then(|v| v.as_bool()) == Some(false),
        Err(_) => true,
    };
    if !refused {
        return;
    }
    log("window/showDocument refused; using applyEdit + CreateFile");
    fall_back_to_apply_edit(state, sender, root);
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
                    {
                        let mut guard = lock_state(state);
                        guard.docs.insert(uri.to_string(), text.to_string());
                        guard.lens_signatures.insert(uri.to_string(), lens_signature(text));
                        // Voltou dentro da carência: era `didClose` espúrio (ou o
                        // usuário reabriu na hora) e as respostas ficam.
                        guard.pending_clear.remove(uri);
                        if uri == result_uri {
                            guard.result_open = true;
                        }
                    }
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
                let signature = lens_signature(text);
                let moved = {
                    let mut guard = lock_state(state);
                    guard.docs.insert(uri.to_string(), text.to_string());
                    guard.lens_signatures.insert(uri.to_string(), signature) != Some(signature)
                };
                // Ao editar um .http de origem, os Code Lens ancorados ficam com
                // a linha defasada, e o Zed só corrige isso se re-pedir os lenses.
                //
                // Mas o refresh é global e *invalida* os lenses de todos os
                // buffers, enquanto o Zed só re-pede os dos editores que ele
                // considera visíveis. Um `.http` escondido atrás da aba de
                // resposta (ou em outro painel) ficava sem nenhum lens até ser
                // reaberto — era assim que o "Send request" desaparecia depois de
                // um tempo de uso, com vários `.http` abertos. Então só pedimos
                // refresh quando a *posição/nome* de alguma requisição mudou;
                // digitar dentro de um corpo JSON não move lens nenhum.
                if uri != result_uri && moved {
                    refresh_code_lens(sender);
                }
            }
        }
        "textDocument/didClose" => {
            if let Some(uri) = not.params.pointer("/textDocument/uri").and_then(|v| v.as_str()) {
                log(format!("<- didClose {uri}"));
                {
                    let mut guard = lock_state(state);
                    guard.docs.remove(uri);
                    guard.lens_signatures.remove(uri);
                    if uri == result_uri {
                        guard.result_open = false;
                    }
                }
                // Fechou o `.http`: as respostas guardadas dele não servem mais
                // para ninguém. O buffer de resultado não tem respostas próprias.
                if uri != result_uri {
                    schedule_response_cleanup(state, uri.to_string());
                }
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
            // Pedido de lens é sinal de vida: o cliente ainda mostra este buffer,
            // então o `didClose` que chegou antes era espúrio e a limpeza das
            // respostas dele é cancelada.
            lock_state(state).pending_clear.remove(&uri);
            let lenses = code_lenses(state, result_uri, &uri);
            let sending = lenses
                .iter()
                .filter(|l| l.command.as_ref().is_some_and(|c| c.command == CMD_NOOP))
                .count();
            log(format!("-> codeLens {uri}: {} lens, {sending} sending", lenses.len()));
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
            log(format!("-> unhandled request: {other}"));
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
    let guard = lock_state(state);
    let text = guard.document_text(uri).unwrap_or_default();
    let (_vars, reqs) = parse_document(&text);
    let keys = request_keys(&reqs);

    reqs.iter()
        .zip(keys)
        .map(|(r, (key, nth))| {
            let range = Range::new(Position::new(r.line, 0), Position::new(r.line, 0));
            if guard.inflight.contains(&(uri.to_string(), r.line)) {
                CodeLens {
                    range,
                    command: Some(LspCommand::new("⏳ Sending…".into(), CMD_NOOP.into(), None)),
                    data: None,
                }
            } else {
                let title = match &r.name {
                    Some(n) => format!("▶ Send request  ({n})"),
                    None => "▶ Send request".into(),
                };
                // O cliente congela os argumentos do comando no momento em que
                // recebe o lens (e ancora só a *posição* na tela), então tudo
                // aqui pode chegar velho no clique. Mandamos várias pistas de
                // identidade, da mais estável para a mais frágil, e quem escolhe
                // é [`resolve_request`]:
                //
                // - `name`: o `# @name`, que sobrevive a mudanças na URL;
                // - `key`: método + URL + nome, que só casa se nada mudou;
                // - `line`: certa enquanto a edição não desloca a requisição;
                // - `method`: corroboração para o caso de cair na linha.
                let args = vec![
                    Value::String(uri.to_string()),
                    Value::from(r.line),
                    Value::String(format!("{key:016x}")),
                    Value::from(nth),
                    Value::String(r.name.clone().unwrap_or_default()),
                    Value::String(r.method.clone()),
                ];
                CodeLens {
                    range,
                    command: Some(LspCommand::new(title, CMD_SEND.into(), Some(args))),
                    data: None,
                }
            }
        })
        .collect()
}

/// Acha a requisição que o clique quis, tolerando argumentos velhos no lens, e
/// devolve `(índice, por que casou)` — o motivo vai para o log.
///
/// As pistas são testadas da mais estável para a mais frágil, porque cada uma
/// morre com um tipo diferente de edição:
///
/// 1. **nome** (`# @name`): sobrevive a qualquer mexida na URL, que é a edição
///    mais comum (ligar/desligar um `&page_size=...` da query multilinha);
/// 2. **identidade** (método + URL + nome): só casa se nada mudou, mas resolve
///    as requisições sem `@name`;
/// 3. **linha**: continua certa enquanto a edição não desloca a requisição —
///    justamente o caso em que 1 e 2 falham. Aqui exigimos o método igual como
///    corroboração: se as linhas andaram, é ele que evita disparar a requisição
///    errada.
///
/// Sem nenhuma casar, a função **desiste** em vez de arriscar — são chamadas de
/// API de verdade.
fn resolve_request(
    reqs: &[HttpRequest],
    line: u32,
    key: Option<u64>,
    nth: Option<u32>,
    name: Option<&str>,
    method: Option<&str>,
) -> Option<(usize, &'static str)> {
    let keys = request_keys(reqs);

    // Desempate entre candidatas: a ocorrência que o lens anotou (só vale quando
    // todas dividem a mesma identidade, senão o contador de todas é 0 e não diz
    // nada) e, por fim, a mais próxima da linha que o clique mandou.
    let tiebreak = |pool: &[usize]| -> usize {
        if pool.len() == 1 {
            return pool[0];
        }
        let one_key = pool.iter().all(|i| keys[*i].0 == keys[pool[0]].0);
        if one_key {
            if let Some(i) = pool.iter().copied().find(|i| Some(keys[*i].1) == nth) {
                return i;
            }
        }
        pool.iter()
            .copied()
            .min_by_key(|i| reqs[*i].line.abs_diff(line))
            .unwrap_or(pool[0])
    };

    // 1) Pelo nome.
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        let named: Vec<usize> = reqs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.name.as_deref() == Some(name))
            .map(|(i, _)| i)
            .collect();
        if !named.is_empty() {
            // Entre homônimas, a identidade completa manda quando ainda casa.
            let exact: Vec<usize> = named
                .iter()
                .copied()
                .filter(|i| Some(keys[*i].0) == key)
                .collect();
            let pool = if exact.is_empty() { &named } else { &exact };
            return Some((tiebreak(pool), "name"));
        }
    }

    // 2) Pela identidade completa.
    if let Some(key) = key {
        let matching: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| *k == key)
            .map(|(i, _)| i)
            .collect();
        if !matching.is_empty() {
            return Some((tiebreak(&matching), "identity"));
        }
    }

    // 3) Pela linha, com o método corroborando quando o lens o informou.
    if let Some(i) = reqs.iter().position(|r| r.line == line) {
        if method.is_none_or(|m| m.eq_ignore_ascii_case(&reqs[i].method)) {
            return Some((i, "line"));
        }
    }

    None
}

fn handle_execute_command(connection: &Connection, state: &Shared, params: ExecuteCommandParams) {
    if params.command == CMD_NOOP {
        return;
    }
    if params.command != CMD_SEND {
        log(format!("unknown command: {}", params.command));
        return;
    }

    let uri = params.arguments.first().and_then(|v| v.as_str()).map(String::from);
    // Linha *do lens*, que pode estar velha — ver [`resolve_request`].
    let clicked_line = params.arguments.get(1).and_then(|v| v.as_u64()).map(|n| n as u32);
    let key = params
        .arguments
        .get(2)
        .and_then(|v| v.as_str())
        .and_then(|s| u64::from_str_radix(s, 16).ok());
    let nth = params.arguments.get(3).and_then(|v| v.as_u64()).map(|n| n as u32);
    let name = params.arguments.get(4).and_then(|v| v.as_str());
    let method = params.arguments.get(5).and_then(|v| v.as_str());
    let (Some(uri), Some(clicked_line)) = (uri, clicked_line) else {
        log("sendRequest without valid arguments");
        return;
    };

    // Localiza a requisição e as variáveis de arquivo a partir do texto guardado.
    let (text, root_path) = {
        let guard = lock_state(state);
        (guard.document_text(&uri), guard.root_path.clone())
    };
    let Some(text) = text else {
        log(format!("document not found: {uri}"));
        return;
    };
    let (file_vars, reqs) = parse_document(&text);
    let resolved = resolve_request(&reqs, clicked_line, key, nth, name, method);
    let Some((index, how)) = resolved else {
        // Silêncio aqui era o pior sintoma do projeto: o botão simplesmente não
        // surtia efeito. Chegar até aqui exige nome, identidade e linha velhos ao
        // mesmo tempo, e nesse ponto o refresh não resolve — o cliente não troca
        // os argumentos de um lens que já desenhou. Por isso a mensagem manda
        // fechar e reabrir o arquivo, que é o que de fato funciona.
        log(format!(
            "request not found at {uri}:{clicked_line} \
             (key={key:?} nth={nth:?} name={name:?} method={method:?})"
        ));
        refresh_code_lens(&connection.sender);
        show_message(
            &connection.sender,
            MessageType::WARNING,
            "This button is stuck on an outdated version of the file. \
             Close and reopen this .http file to make the buttons work again.",
        );
        return;
    };
    let req = reqs[index].clone();
    // A linha que vale é a atual, não a do lens: é ela que ancora o `⏳` e a
    // guarda de clique duplo.
    let line = req.line;
    if line != clicked_line {
        // O lens deslocou. Vale pedir os lenses de novo — em clientes que
        // atualizam os argumentos, o próximo clique já vem certo.
        log(format!(
            "outdated lens at {uri}: click at {clicked_line}, request at {line} (matched by {how})"
        ));
        refresh_code_lens(&connection.sender);
    } else if key != Some(request_key(&req)) {
        // A linha estava certa, mas o resto dos argumentos não: foi a requisição
        // que mudou de texto (mexer na query multilinha faz isso a cada edição).
        // Registrar ajuda a distinguir os dois desencontros no log.
        log(format!("lens with stale arguments at {uri}:{line}, matched by {how}"));
    }

    // Impede empilhar duas execuções da mesma requisição. A garantia tem que
    // estar aqui, e não na aparência do Code Lens: o Zed só desenha os lenses
    // que ele pede, e ele para de pedir os do `.http` de origem assim que o
    // painel de resultado vira o editor ativo — então o botão pode continuar
    // dizendo "Send request" mesmo com a requisição em andamento.
    // `insert` devolve false quando a chave já estava lá.
    if !lock_state(state).inflight.insert((uri.clone(), line)) {
        let what = req
            .name
            .clone()
            .unwrap_or_else(|| format!("{} {}", req.method, req.url));
        log(format!("click ignored, already running: {what} ({uri}:{line})"));
        show_message(
            &connection.sender,
            MessageType::WARNING,
            format!("⏳ {what} is already running — wait for the response."),
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
