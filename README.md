# HTTP Request Client (extensão para o Zed)

Extensão para o [Zed](https://zed.dev) que adiciona suporte à linguagem `.http`
(e `.rest`) — no mesmo espírito do REST Client do VS Code — permitindo escrever
requisições HTTP em arquivos de texto simples, com destaque de sintaxe e
execução das requisições direto do editor.

Arquivo de exemplo usado para desenvolvimento/testes: [`api.http`](./api.http),
com variáveis lidas de um `.env` local — copie o modelo antes de usar:

```sh
cp .env.example .env
```

O `.env` é procurado a partir da pasta do próprio arquivo `.http`, subindo até
a raiz do workspace, então é possível manter um `.env` por ambiente
(ex.: `.rest/prd/.env`, `.rest/local/.env`). O mais próximo do arquivo tem
prioridade. O `.env` em si não é versionado (ver [`.gitignore`](./.gitignore)).

## Status atual

- [x] **Destaque de sintaxe** para `.http`/`.rest`
- [x] Language Server nativo em Rust (execução real das requisições)
- [x] Code Lens "▶ Send request" acima de cada método HTTP
- [x] Indicador de carregamento no lugar do Code Lens durante a requisição
- [x] Exibição do resultado (status line + headers + body formatado) em uma aba
      ao lado
- [x] Resolução do binário do language server (settings do Zed → build local do
      repositório → `$PATH`)
- [ ] Distribuição: baixar o binário do release do GitHub em vez de exigir
      `cargo install` (ver `src/lib.rs`)

### Destaque de sintaxe

Implementado de forma declarativa, usando a gramática
[tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http)
(MIT), a mesma usada pelo `rest.nvim`. Ela reconhece a estrutura de um arquivo
`.http`: método, URL, versão HTTP, cabeçalhos, corpo, comentários, separadores
de requisição (`###`), declarações de variável (`@NOME = valor`) e
interpolações (`{{NOME}}`).

O que é colorido:

| Elemento                                   | Exemplo no `api.http`                        |
|---------------------------------------------|-----------------------------------------------|
| Método HTTP                                 | `POST`, `GET`                                 |
| URL                                          | `{{HOST}}/v1/oauth/login`                     |
| Nome do cabeçalho                            | `content-type`, `Authorization`               |
| Interpolação de variável                     | `{{USERNAME}}`, `{{oauthLogin.response...}}`  |
| Declaração de variável                       | `@HOST = ...`                                 |
| Metadado de comentário (`# @name`)           | `# @name oauthLogin`                          |
| Separador de requisição                      | `###`                                         |
| Status/versão HTTP (em respostas coladas)    | `HTTP/1.1`, `200`, `OK`                       |
| Corpo JSON/XML                               | injetado com o grammar nativo de JSON/XML do Zed |
| Query param comentado                        | `    # &sort=asc`                             |

Corpos `json`/`xml` são destacados recursivamente via *injection* usando os
próprios grammars de JSON e XML já embutidos no Zed — o mesmo mecanismo que
faz blocos de código dentro de Markdown ficarem coloridos.

O grammar usado é um **fork** do upstream, em
[`grammars-src/`](./grammars-src/README.md), com dois patches:

- uma linha comentada dentro de uma query string multilinha era engolida pela
  URL e ficava com a cor de URL, indistinguível de um parâmetro ativo; agora
  vira um nó `(comment)`;
- indentação com **TAB** não era reconhecida como espaço em branco (TAB é
  `\p{Cc}`, e o grammar usava `\p{Zs}`), o que fazia cada linha de uma query
  string multilinha virar uma requisição solta. Isso afetava o arquivo inteiro,
  não só as query strings.

## Estrutura do projeto

Workspace Cargo com dois crates: a extensão em WASM (que o Zed carrega e que
apenas inicia o language server) e o language server nativo (que faz todo o
trabalho — parse, resolução de variáveis e as requisições HTTP).

```
.
├── extension.toml              # manifesto da extensão, grammar e language server
├── Cargo.toml                  # workspace: crate da extensão (cdylib) + lsp-server
├── src/
│   └── lib.rs                  # extensão WASM: só localiza/inicia o lsp-server
├── lsp-server/
│   └── src/main.rs             # language server: parser do .http, variáveis,
│                               # execução HTTP (ureq), code lens e comandos
├── languages/
│   └── http/
│       ├── config.toml         # associação de .http/.rest à linguagem
│       ├── highlights.scm      # regras de destaque de sintaxe
│       └── injections.scm      # injeção de JSON/XML dentro do corpo
├── grammars-src/
│   └── tree-sitter-http/       # fork do grammar (repo git próprio); é o que
│                               # o extension.toml referencia. Não confundir
│                               # com grammars/, que é o checkout gerado pelo Zed
├── api.http                    # arquivo de exemplo/documentação
├── example.csv                 # usado pelo exemplo de upload (`< ./example.csv`)
└── .env.example                # modelo das variáveis usadas pelo api.http
```

## Como testar localmente no Zed

1. Compile o language server: `cargo build -p http_request_client_lsp`.
   Para usar a extensão em **outros** projetos, instale-o no `$PATH`:
   `cargo install --path lsp-server`.
2. Habilite os Code Lens no `settings.json` do Zed: `"code_lens": "on"`.
3. Abra o Zed **por um terminal que tenha o `cargo` no `PATH`** — o Zed herda o
   ambiente de quem o iniciou e precisa dele para compilar a dev extension.
4. `zed: install dev extension` (paleta de comandos) e selecione esta pasta.
5. Copie o `.env` (`cp .env.example .env`) e abra o `api.http` — o destaque de
   sintaxe é aplicado e o botão "▶ Send request" aparece acima de cada
   requisição.

Recomendado: `"autosave": "on_focus_change"` no `settings.json`. A aba de
resultado é aberta via `workspace/applyEdit` e nasce "suja" (não salva); o
autosave a deixa limpa, e é isso que permite que as respostas seguintes sejam
atualizadas em disco sem roubar o foco do editor.

### Como o binário do language server é encontrado

A extensão WASM ([`src/lib.rs`](./src/lib.rs)) resolve o binário na seguinte
ordem, da fonte mais explícita para a mais automática:

1. o caminho configurado no `settings.json` do Zed:

   ```json
   {
     "lsp": {
       "http-request-client": {
         "binary": { "path": "/caminho/para/http-request-client-lsp" }
       }
     }
   }
   ```

2. `target/debug/http-request-client-lsp` do próprio repositório da extensão,
   quando é ele que está aberto no Zed — assim o ciclo de desenvolvimento
   (`cargo build` → reiniciar o language server) funciona sem instalar nada;
3. o `$PATH`, via `worktree.which(...)` — é o caso ao usar a extensão em outros
   projetos, depois de `cargo install --path lsp-server`.

Se nenhuma das três funcionar, o Zed mostra uma mensagem com essas opções.

## Funcionalidade "Send request"

- Um Code Lens `▶ Send request  (nome)` aparece na linha do método HTTP de cada
  requisição (ex.: acima de `POST {{HOST}}/v1/oauth/login`). O nome vem de
  `# @name`, quando presente.
- Ao clicar, a requisição roda numa thread separada (o editor não trava) e o
  progresso aparece em três lugares, em ordem de confiabilidade:
  1. **`# ⏳ Enviando…` no painel de resultado**, imediatamente, no lugar da
     resposta anterior. É o feedback principal;
  2. **barra de status do Zed**, via `$/progress` (`window/workDoneProgress/create`
     + `begin`/`end`). Não depende de layout nem de foco;
  3. **o Code Lens vira `⏳ Enviando…`** — quando o Zed pede.

  **O `⏳` no Code Lens depende do layout dos painéis.** O Zed só pede os lenses
  dos buffers *visíveis* de cada editor (`visible_buffers`), então:

  - o painel de resultado **num split ao lado** do `.http` → os dois editores
    ficam visíveis, o Zed pede os lenses dos dois e o botão troca para `⏳` e
    volta normalmente;
  - o painel de resultado como **aba no mesmo painel**, por cima do `.http` →
    o editor do `.http` fica escondido, o Zed para de pedir os lenses dele e o
    botão congela em `▶ Send request` mesmo durante a requisição.

  No log a diferença é direta: no primeiro caso cada refresh gera um
  `-> codeLens` para o `.http` **e** outro para o arquivo de resultado; no
  segundo, só para o arquivo de resultado. Isso não é contornável pelo
  servidor — daí os outros dois indicadores acima, e a trava abaixo.
- **Uma requisição por vez, por linha.** Clicar de novo enquanto ela está em
  andamento não dispara uma segunda: o clique é barrado no servidor (`inflight`)
  e vira um aviso `⏳ <nome> já está em andamento`. A trava tem que ficar aí
  justamente porque o botão nem sempre chega a mudar para `⏳`. Requisições em
  linhas diferentes continuam podendo rodar em paralelo.
- Quando o servidor consegue servir o `⏳`, ele o segura por no mínimo 400 ms
  (`MIN_LOADING`): o Zed espera 50 ms de debounce + 30 ms antes de pedir os
  lenses de volta, e um `codeLens/refresh` novo **substitui** o pedido pendente
  em vez de enfileirá-lo — então, numa requisição de poucos ms, o refresh do
  fim cancelava o do começo. A espera só atrasa o botão voltar ao normal: a
  resposta já foi escrita antes dela.
- A resposta é entregue por escrita em disco quando o painel de resultado já
  estava aberto antes da requisição, e por `applyEdit` quando foi o próprio
  `⏳ Enviando…` que abriu a aba — nesse caso o buffer ainda pode estar "sujo",
  e o watcher do Zed ignoraria a escrita em disco.
- A requisição é resolvida e executada pelo `lsp-server` nativo (não pela
  extensão WASM, que não tem acesso à rede):
  - variáveis `{{NOME}}` são resolvidas a partir de declarações `@NOME = valor`
    no arquivo e de variáveis do `.env` (`{{$dotenv NOME}}`). A resolução é
    recursiva, então `@HOST = {{$dotenv HOST}}` funciona;
  - o `.env` é procurado a partir da pasta do arquivo `.http`, subindo até a
    raiz do workspace — o mais próximo tem prioridade, o que permite um `.env`
    por ambiente (ex.: `.rest/prd/.env`);
  - referências encadeadas a respostas anteriores são resolvidas a partir do
    cache de respostas da sessão atual (em memória; some ao reiniciar o
    language server), em três formas:
    - `{{nome.response.body.caminho}}` — navega o JSON do corpo
      (ex.: `{{oauthLogin.response.body.access_token}}`);
    - `{{nome.response.headers.Header}}` — valor de um cabeçalho da resposta
      (match case-insensitive, ex.: `{{oauthLogin.response.headers.content-type}}`);
    - `{{nome.response.status}}` — código de status HTTP (ex.: `200`).
  - inclusão de arquivo no corpo (estilo REST Client):
    - `< caminho` insere o conteúdo do arquivo cru (caminho relativo ao `.http`);
    - `<@ caminho` insere o conteúdo e resolve `{{...}}` dentro dele.
  - se sobrar algum `{{...}}` sem resolver, a requisição **não** é enviada: o
    resultado traz a lista das variáveis faltantes, em vez de um erro obscuro
    do cliente HTTP.
- O parser tolera os padrões comuns de arquivos reais: query string em várias
  linhas (linhas iniciadas por `?` ou `&`), comentários entre os cabeçalhos,
  parâmetros de query comentados e comentários depois do corpo (que não entram
  no corpo enviado).
- Com **vários `.http` abertos ao mesmo tempo**, todos mostram os lenses. Duas
  defesas no servidor garantem isso, porque o cliente é a parte frágil aqui:
  - quem desenha os lenses é o *editor*, e ele só busca os buffers já
    registrados e visíveis nele. Duas corridas fazem essa busca cair no vazio,
    sem nada reagendá-la depois: abrir um segundo `.http` (a busca chega antes
    do registro do buffer) e abrir o Zed com `.http` já abertos (a restauração
    do workspace é assíncrona, e a busca pode acontecer antes de o editor
    existir — o servidor até responde os lenses, dá para ver no log, e a aba
    fica sem botões até ser fechada e reaberta). O servidor manda
    `workspace/codeLens/refresh` em 50 ms, 400 ms, 1,5 s e 4 s depois de cada
    `didOpen` (`LENS_NUDGES_MS`) para cobrir as duas;
  - os lenses não dependem do bookkeeping de `didOpen`/`didClose`: se o texto de
    um documento não estiver em memória, ele é lido do disco. Um `didClose` a
    mais (abas de preview, o mesmo arquivo em dois painéis) deixaria a aba muda
    até ser reaberta.

  O log fica em `/tmp/http-request-client-lsp-<nome-do-workspace>.log` — um por
  projeto, porque o Zed sobe um language server por projeto aberto e com um
  caminho fixo os logs se misturavam. Ele registra uma linha
  `-> codeLens <uri>: N lens, M enviando` por requisição de lens, útil para
  diferenciar "o servidor não respondeu" de "o cliente não pediu", e para ver
  se o `⏳` chegou a ser servido.
- O resultado é formatado como status line + cabeçalhos + corpo (pretty-print
  quando JSON) e escrito em `/tmp/requests/<nome-do-workspace>.http`:

  ```
  HTTP/1.1 200 OK
  Date: Wed, 22 Jul 2026 19:49:13 GMT
  Content-Type: application/json; charset=utf-8
  ...

  {
    "message": "Welcome to api application."
  }
  ```

  Se o arquivo já estiver aberto, ele é atualizado em disco e o file watcher do
  Zed recarrega o buffer — sem roubar o foco, o que permite deixá-lo num split
  ao lado do `.http`. Se estiver fechado, é aberto via `workspace/applyEdit`
  (a única forma de o Zed abrir uma aba a pedido do language server, já que ele
  não implementa `window/showDocument`).

  O arquivo fica **fora do projeto**, em `/tmp/requests/` (a pasta é criada
  quando não existe), com o nome do workspace — assim ele não suja o repositório
  nem precisa de `.gitignore`, e dois projetos abertos ao mesmo tempo não
  disputam o mesmo arquivo. Estar fora do worktree não atrapalha: testei, e o
  Zed cria um worktree invisível de arquivo único para ele, registra o language
  server nele e observa o arquivo normalmente — a escrita em disco gera
  `didChange` como antes. Consequência esperada de morar em `/tmp`: as respostas
  não sobrevivem a um boot.

### Limitação conhecida (destaque de sintaxe)

A gramática `tree-sitter-http` reconhece interpolações no formato
`{{identificador}}` (sem espaço) — cobre bem casos como
`{{oauthLogin.response.body.access_token}}`. Já variáveis de processador com
argumento e espaço, como `{{$dotenv HOST}}`, não são reconhecidas como o nó
`variable` e por isso não recebem o destaque de colchetes/identificador;
o texto continua correto e funcional, só não fica colorido como variável.
Ajustar isso exigiria estender a gramática.

**Comentários logo após o corpo (body) quebram o destaque do restante do
arquivo.** A gramática `tree-sitter-http` (mesmo na versão mais recente) aceita
comentários **antes** dos cabeçalhos, mas não lida bem com comentários **depois
do body** de uma requisição, antes do próximo `###`. Como a recuperação de erro
da gramática é fraca, um único caso desses gera um nó de erro que se propaga e
"apaga" as cores de tudo que vem depois no arquivo. Exemplo que quebra:

```http
# @name backupMade
POST {{HOST}}/v1/backup
content-type: {{CONTENT_TYPE}}

{
  "model_name": "receivable_units"
}

# model_name: nome da tabela   <- comentário após o body: quebra o highlight
###
```

Workaround: colocar os comentários de documentação **antes** do body (junto aos
cabeçalhos, onde a gramática os aceita):

```http
# @name backupMade
# model_name: nome da tabela   <- comentário antes do body: OK
POST {{HOST}}/v1/backup
content-type: {{CONTENT_TYPE}}

{
  "model_name": "receivable_units"
}
###
```

Isso afeta apenas o **destaque de sintaxe**; a execução da requisição
(`Send request`), o parsing e a resolução de variáveis funcionam normalmente
mesmo com comentários após o body. A correção definitiva exigiria estender a
regra de comentários e a recuperação de erro da gramática — o fork em
[`grammars-src/`](./grammars-src/README.md) já é o lugar para isso (foi onde o
caso dos comentários em query params foi resolvido).

## Créditos

- Grammar: [rest-nvim/tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http) (MIT),
  usado através do fork em [`grammars-src/`](./grammars-src/README.md)
