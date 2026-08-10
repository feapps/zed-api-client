//! O que um clique em "Send request" manda para o cliente — e o que ele **não**
//! pode mandar.
//!
//! Dois sintomas moram aqui, os dois só visíveis na conversa com o cliente (daí
//! o servidor rodar de verdade e o teste fazer o papel do Zed, inclusive
//! recusando `window/showDocument` com `-32601`, como ele faz):
//!
//! - **aba duplicada**: cada `applyEdit` num arquivo que o cliente ainda não
//!   abriu faz o Zed abrir uma aba, e dois em sequência — antes de o `didOpen`
//!   do primeiro chegar — abrem duas abas da mesma resposta;
//! - **botão sumindo**: um `workspace/codeLens/refresh` invalida os lenses de
//!   *todos* os buffers e o Zed só re-pede os do editor visível, que durante uma
//!   requisição é a aba de resposta. Um refresh disparado pelo clique deixava o
//!   `.http` de origem sem nenhum botão até ser fechado e reaberto.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Quanto tempo esperar as mensagens de um clique. Folgado de propósito: uma
/// duplicata que só aparecesse depois disso continuaria sendo uma aba a mais.
const COLLECT: Duration = Duration::from_millis(2_000);

/// Quanto esperar, depois do `didOpen`, para os pedidos tardios de Code Lens
/// (`LENS_NUDGES_MS`, o último em 4 s) já terem passado. Eles são legítimos e
/// não têm nada a ver com o clique — mas caem no mesmo canal.
const AFTER_NUDGES: Duration = Duration::from_millis(4_500);

/// Servidor HTTP mínimo, alvo das requisições do `.http` do teste.
///
/// Responde na hora: é a resposta rápida que faz os dois `applyEdit` do bug
/// original caírem no mesmo milissegundo.
fn spawn_http_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { return };
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => seen.push(byte[0]),
                }
            }
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/plain\r\n\
                  Content-Length: 2\r\n\
                  Connection: close\r\n\r\nok",
            );
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// O servidor de verdade, falando LSP por stdio.
struct Server {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
}

impl Server {
    fn start(root: &str) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_http-request-client-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lsp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });

        let mut server = Server { child, stdin, incoming, next_id: 1 };
        let id = server.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": format!("file://{root}"),
                // Sem `window.showDocument`: é o que o Zed anuncia, e o que faz
                // o servidor tentar a requisição para descobrir na prática.
                "capabilities": {},
            }),
        );
        server.wait_response(id);
        server.notify("initialized", json!({}));
        server
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn send(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).expect("serialize");
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write");
        self.stdin.flush().expect("flush");
    }

    /// Consome mensagens até a resposta de `id`, respondendo o que o servidor
    /// pedir pelo caminho.
    fn wait_response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + COLLECT;
        loop {
            let msg = self.recv(deadline).expect("no response before the deadline");
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) && msg.get("method").is_none() {
                return msg;
            }
            self.answer(&msg);
        }
    }

    /// Clica no "Send request" da requisição da linha `line` e devolve os
    /// `workspace/applyEdit` que o servidor mandou por causa disso.
    fn click(&mut self, uri: &str, line: u32) -> Vec<Value> {
        self.click_all(uri, line)
            .into_iter()
            .filter(|m| m.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit"))
            .collect()
    }

    /// O mesmo clique, devolvendo **tudo** o que o servidor mandou — para os
    /// testes que verificam o que não pode aparecer.
    fn click_all(&mut self, uri: &str, line: u32) -> Vec<Value> {
        let id = self.request(
            "workspace/executeCommand",
            json!({"command": "http.sendRequest", "arguments": [uri, line]}),
        );

        let mut msgs = Vec::new();
        // A coleta começa antes da resposta do comando, e não depois: o
        // `handle_execute_command` roda inteiro *antes* de o servidor responder,
        // então tudo o que ele mandar direto do clique — o refresh do bug
        // antigo, por exemplo — sai nesta primeira janela.
        let deadline = Instant::now() + COLLECT;
        loop {
            let msg = self.recv(deadline).expect("no response before the deadline");
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) && msg.get("method").is_none() {
                break;
            }
            self.answer(&msg);
            msgs.push(msg);
        }
        // Depois, o que a requisição em background produzir.
        let deadline = Instant::now() + COLLECT;
        while let Some(msg) = self.recv(deadline) {
            self.answer(&msg);
            msgs.push(msg);
        }
        msgs
    }

    /// Consome (e responde) tudo o que chegar em `how_long`, sem guardar nada.
    fn drain(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while let Some(msg) = self.recv(deadline) {
            self.answer(&msg);
        }
    }

    /// Responde uma requisição do servidor como o Zed responderia.
    fn answer(&mut self, msg: &Value) {
        let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(|v| v.as_str()))
        else {
            return;
        };
        let reply = match method {
            // O Zed não implementa: é este `-32601` que empurra o servidor para
            // o caminho de `applyEdit` + `CreateFile`.
            "window/showDocument" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Unrecognized method `window/showDocument`"},
            }),
            "workspace/applyEdit" => {
                json!({"jsonrpc": "2.0", "id": id, "result": {"applied": true}})
            }
            _ => json!({"jsonrpc": "2.0", "id": id, "result": null}),
        };
        self.send(reply);
    }

    fn recv(&self, deadline: Instant) -> Option<Value> {
        match self.incoming.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(reader: &mut BufReader<impl Read>) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            len = v.parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Quantas operações de `CreateFile` (as que abrem aba) tem um `applyEdit`.
fn creates(edit: &Value) -> usize {
    edit.pointer("/params/edit/documentChanges")
        .and_then(|v| v.as_array())
        .map(|ops| ops.iter().filter(|op| op.get("kind").is_some()).count())
        .unwrap_or(0)
}

/// Um workspace por teste: o diretório de artefatos (e com ele o arquivo de
/// resultado) sai do nome dele, e os testes rodam em paralelo.
fn workspace_root(test: &str) -> String {
    let dir = std::env::temp_dir().join(format!("http-client-test-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.to_string_lossy().into_owned()
}

/// Um clique com a aba fechada tem que mandar **um** `applyEdit`.
///
/// Dois — o do "⏳ Sending…" e o da resposta — chegam ao Zed antes do `didOpen`
/// do primeiro, e ele abre uma aba para cada.
#[test]
fn one_click_opens_the_result_tab_once() {
    let url = spawn_http_server();
    let root = workspace_root("once");
    let mut server = Server::start(&root);

    let uri = format!("file://{root}/api.http");
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri,
            "languageId": "http",
            "version": 1,
            "text": format!("GET {url}\n"),
        }}),
    );

    let edits = server.click(&uri, 0);
    assert_eq!(edits.len(), 1, "one applyEdit per click, got: {edits:#?}");
    assert_eq!(creates(&edits[0]), 1, "the single applyEdit has to open the tab");
}

/// O mesmo com o servidor de destino fora do ar.
///
/// É o caso mais fácil de reproduzir: o `Connection refused` volta em
/// microssegundos, então os dois `applyEdit` do bug caiam no mesmo
/// milissegundo. Com o destino respondendo, a latência da requisição os
/// separava e a segunda aba só aparecia de vez em quando — foi o que fez o bug
/// parecer corrigido antes da hora.
#[test]
fn a_refused_connection_opens_the_result_tab_once() {
    let root = workspace_root("refused");
    let mut server = Server::start(&root);
    let uri = open_http_file(&mut server, &root, &closed_port_url());

    let edits = server.click(&uri, 0);
    assert_eq!(edits.len(), 1, "one applyEdit per click, got: {edits:#?}");
    assert_eq!(creates(&edits[0]), 1, "the error still has to be shown somewhere");
}

/// URL de uma porta onde ninguém atende: liga e desliga um listener só para
/// receber do sistema um número de porta que estava livre agora.
fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}/")
}

/// Com a aba já aberta, nenhum clique pode recriar o arquivo: o `CreateFile`
/// apaga e recria, e o Zed abre outra aba em cima da que já estava lá.
#[test]
fn a_second_click_does_not_recreate_the_tab() {
    let url = spawn_http_server();
    let root = workspace_root("second-click");
    let mut server = Server::start(&root);
    let uri = open_http_file(&mut server, &root, &url);

    let first = server.click(&uri, 0);
    let result_uri = result_uri(&first);
    open_tab(&mut server, &result_uri);

    let second = server.click(&uri, 0);
    let created: usize = second.iter().map(creates).sum();
    assert_eq!(created, 0, "the tab is open; nothing to create: {second:#?}");
}

/// Fechar a aba de propósito e clicar de novo tem que reabri-la — sem isso a
/// resposta vai para um arquivo que ninguém está vendo e o clique parece não
/// fazer nada. E reabrir é *um* `applyEdit`, pelo mesmo motivo do primeiro
/// teste.
#[test]
fn closing_the_tab_and_clicking_again_reopens_it_once() {
    let url = spawn_http_server();
    let root = workspace_root("reopen");
    let mut server = Server::start(&root);
    let uri = open_http_file(&mut server, &root, &url);

    let first = server.click(&uri, 0);
    let result_uri = result_uri(&first);
    open_tab(&mut server, &result_uri);
    server.notify("textDocument/didClose", json!({"textDocument": {"uri": result_uri}}));

    let again = server.click(&uri, 0);
    assert_eq!(again.len(), 1, "one applyEdit per click, got: {again:#?}");
    assert_eq!(creates(&again[0]), 1, "the closed tab has to come back");
}

fn open_http_file(server: &mut Server, root: &str, url: &str) -> String {
    let uri = format!("file://{root}/api.http");
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri,
            "languageId": "http",
            "version": 1,
            "text": format!("GET {url}\n"),
        }}),
    );
    uri
}

/// A aba que o `CreateFile` do clique pediu.
fn result_uri(edits: &[Value]) -> String {
    edits
        .first()
        .and_then(|e| e.pointer("/params/edit/documentChanges/0/uri"))
        .and_then(|v| v.as_str())
        .expect("result uri")
        .to_string()
}

/// O `didOpen` com que o Zed avisa que a aba de resultado abriu.
fn open_tab(server: &mut Server, uri: &str) {
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri,
            "languageId": "http",
            "version": 1,
            "text": "",
        }}),
    );
}

/// Servidor que aceita a conexão e nunca responde: segura a requisição em voo
/// pelo tempo do teste, para dar tempo de perguntar os lenses no meio dela.
fn spawn_stalled_http_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(sock) = stream else { return };
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(30));
                drop(sock);
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// Um clique não pode pedir `workspace/codeLens/refresh`.
///
/// O refresh invalida os lenses de *todos* os buffers e o Zed só re-pede os do
/// editor visível — que, logo depois de um clique, é a aba de resposta. Os dois
/// refreshes que o antigo `⏳ Sending…` exigia (um para pôr o `⏳`, outro para
/// tirar) eram exatamente o que apagava o "Send request" do `.http` de origem,
/// que só voltava fechando e reabrindo o arquivo.
#[test]
fn a_click_never_asks_for_a_code_lens_refresh() {
    let url = spawn_http_server();
    let root = workspace_root("no-refresh");
    let mut server = Server::start(&root);
    let uri = open_http_file(&mut server, &root, &url);
    // Os pedidos tardios do `didOpen` são legítimos e não são o alvo aqui.
    server.drain(AFTER_NUDGES);

    let msgs = server.click_all(&uri, 0);
    let refreshes: Vec<&Value> = msgs
        .iter()
        .filter(|m| m.get("method").and_then(|v| v.as_str()) == Some("workspace/codeLens/refresh"))
        .collect();
    assert!(
        refreshes.is_empty(),
        "a click must not invalidate the lenses: {refreshes:#?}"
    );
}

/// Com a requisição em voo, o botão continua "Send request" — e continua sendo
/// o comando de verdade, não um placeholder.
///
/// É a outra metade do teste acima: sem indicador no lens, não há o que
/// atualizar, e sem o que atualizar não há refresh. Quem mostra "está rodando"
/// é o `$/progress` na barra de status e o placeholder da aba de resposta.
#[test]
fn the_button_stays_send_request_while_the_request_runs() {
    let url = spawn_stalled_http_server();
    let root = workspace_root("stays-clickable");
    let mut server = Server::start(&root);
    let uri = open_http_file(&mut server, &root, &url);

    // O servidor marca a requisição como em andamento antes de responder ao
    // comando, então depois desta linha ela já está registrada.
    let id = server.request(
        "workspace/executeCommand",
        json!({"command": "http.sendRequest", "arguments": [&uri, 0]}),
    );
    server.wait_response(id);

    let id = server.request("textDocument/codeLens", json!({"textDocument": {"uri": &uri}}));
    let lenses = server.wait_response(id);
    assert_eq!(
        lenses.pointer("/result/0/command/title").and_then(|v| v.as_str()),
        Some("▶ Send request"),
        "got: {lenses:#?}"
    );
    assert_eq!(
        lenses.pointer("/result/0/command/command").and_then(|v| v.as_str()),
        Some("http.sendRequest"),
        "got: {lenses:#?}"
    );
}
