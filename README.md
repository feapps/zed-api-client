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
      repositório → `$PATH` → download do release)
- [x] Distribuição: baixa o binário do GitHub Release automaticamente, sem
      exigir `cargo install` (ver `src/lib.rs`)

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
   Para usar a extensão em **outros** projetos a partir deste checkout, instale-o
   no `$PATH`: `cargo install --path lsp-server`. (Quem instala a extensão pela
   loja do Zed não precisa disso — o binário é baixado do release; ver
   [Como o binário do language server é encontrado](#como-o-binário-do-language-server-é-encontrado).)
2. Habilite os Code Lens no `settings.json` do Zed: `"code_lens": "on"`.
3. Abra o Zed **por um terminal que tenha o `cargo` no `PATH`** — o Zed herda o
   ambiente de quem o iniciou e precisa dele para compilar a dev extension.
4. `zed: install dev extension` (paleta de comandos) e selecione esta pasta.
5. Copie o `.env` (`cp .env.example .env`) e abra o `api.http` — o destaque de
   sintaxe é aplicado e o botão "▶ Send request" aparece acima de cada
   requisição.

Recomendado: `"autosave": "on_focus_change"` no `settings.json`. Ele só é
necessário no caminho de fallback (clientes que não tratam
`window/showDocument`), em que a aba de resultado é aberta via
`workspace/applyEdit` e nasce "suja" (não salva): o autosave a deixa limpa, e é
isso que permite que as respostas seguintes sejam atualizadas em disco sem roubar
o foco do editor.

### Se a instalação falhar em `failed to compile grammar 'http'`

O Zed mantém um checkout do grammar em `grammars/` (gerado por ele, ignorado no
git) e **se recusa a reaproveitá-lo quando o `repository` do `extension.toml`
mudou**, com esta mensagem:

```
grammar directory '.../grammars/http' already exists,
but is not a git clone of 'https://github.com/feapps/tree-sitter-http'
```

É o caso de quem instalou a extensão quando o `[grammars.http] repository`
apontava para um caminho local. Como `grammars/` é artefato regenerável, apague
e instale de novo:

```sh
rm -rf grammars/
```

O Zed clona o grammar do zero na próxima instalação. Vale o mesmo sempre que
`repository` ou `rev` mudarem.

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
   projetos, depois de `cargo install --path lsp-server`;
4. o binário publicado no **GitHub Release** do repositório, baixado
   automaticamente para a plataforma atual — é o caminho de quem instala a
   extensão pela loja do Zed, sem precisar de Rust nem `cargo` na máquina.

O download do passo 4 usa o asset
`http-request-client-lsp-<os>-<arch>.gz` do último release (`os` ∈
`macos`/`linux`/`windows`, `arch` ∈ `aarch64`/`x86_64`/`x86`), publicado pelo
workflow [`.github/workflows/release.yml`](./.github/workflows/release.yml). O
binário é guardado num diretório versionado, reaproveitado nas execuções
seguintes, e versões antigas são removidas. O progresso aparece na UI do Zed
como status de instalação do language server.

Se nenhuma das quatro funcionar, o Zed mostra o motivo da falha.

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
- A resposta é sempre escrita em disco; se o cliente não tem o buffer aberto,
  um `window/showDocument` (sem roubar o foco) mostra a aba. No fallback por
  `applyEdit`, a primeira resposta vai pelo próprio edit — nesse caminho o buffer
  ainda pode estar "sujo", e o watcher do Zed ignoraria a escrita em disco.
- A requisição é resolvida e executada pelo `lsp-server` nativo (não pela
  extensão WASM, que não tem acesso à rede):
  - variáveis `{{NOME}}` são resolvidas a partir de declarações `@NOME = valor`
    no arquivo e de variáveis do `.env` (`{{$dotenv NOME}}`). A resolução é
    recursiva, então `@HOST = {{$dotenv HOST}}` funciona;
  - o `.env` é procurado a partir da pasta do arquivo `.http`, subindo até a
    raiz do workspace — o mais próximo tem prioridade, o que permite um `.env`
    por ambiente (ex.: `.rest/prd/.env`);
  - referências encadeadas a respostas anteriores são resolvidas a partir do
    cache de respostas, que é **persistido** em
    `<dir-privado>/responses.json` (`0600`), em três formas:
    - `{{nome.response.body.caminho}}` — navega o JSON do corpo
      (ex.: `{{login.response.body.json.key}}`);
    - `{{nome.response.headers.Header}}` — valor de um cabeçalho da resposta
      (match case-insensitive, ex.: `{{login.response.headers.content-type}}`);
    - `{{nome.response.status}}` — código de status HTTP (ex.: `200`).

    Esse cache é **por ambiente**, sendo o ambiente a pasta do arquivo `.http` —
    o mesmo critério usado para achar o `.env`. Então um `# @name login` em
    `.rest/hml/api.http` e outro em `.rest/prd/api.http` guardam tokens
    independentes: autenticar num ambiente não derruba a sessão do outro. Dois
    arquivos `.http` na **mesma** pasta continuam compartilhando as respostas,
    o que permite separar (por exemplo) `login.http` e `pedidos.http` sem
    precisar repetir o login. Consequência: mover um `.http` para outra pasta
    zera o encadeamento dele, porque mudou de ambiente.

    O cache é persistido porque o Zed **para e sobe o language server no meio da
    sessão** (derruba quando o último `.http` fecha, e também reinicia sozinho com
    arquivos abertos — dá para ver no log um `=== starting ===` sem nenhum
    `didClose` antes). Enquanto ele vivia só em memória, esse ciclo — invisível
    para quem está usando — apagava o token do `# @name oauthLogin`, e as
    requisições seguintes falhavam com "variáveis não resolvidas" sem nada na tela
    explicando por quê.

    **Fechar um `.http` apaga as respostas guardadas dele** (memória e disco), com
    três ressalvas que existem para não apagar o que ainda está em uso:

    - o ambiente é a *pasta*, então ele só é limpo quando **nenhum outro `.http`
      aberto** o compartilha — fechar `login.http` não derruba a sessão do
      `pedidos.http` aberto ao lado;
    - a limpeza espera 3 s (`CLOSE_GRACE`) e é cancelada se o arquivo voltar
      (`didOpen`) ou der sinal de vida (um pedido de Code Lens) nesse intervalo —
      é o filtro para o `didClose` espúrio do Zed;
    - um reinício do servidor **sem** `didClose` não apaga nada: só o que estava
      pendente de limpeza é descartado no encerramento (`flush_pending_clears`).

    Consequência assumida: fechar todos os `.http` e reabrir depois exige refazer
    o login. Respostas maiores que 512 KiB (`MAX_RESPONSE_ENTRY_BYTES`) ficam só em
    memória — dá para encadear com elas na sessão, mas elas não vão para o disco,
    para uma listagem de 2 MB não impedir o token de 700 bytes de ser salvo (era o
    que acontecia com um teto só no total: `respostas não persistidas` no log e
    nada era gravado). Quando não sobra nenhuma resposta, o `responses.json` é
    removido.
  - inclusão de arquivo no corpo (estilo REST Client):
    - `< caminho` insere o conteúdo do arquivo cru (caminho relativo ao `.http`);
    - `<@ caminho` insere o conteúdo e resolve `{{...}}` dentro dele.

    Por segurança, a leitura é **confinada à raiz do workspace**: o caminho é
    canonicalizado (resolvendo `..` e symlinks) e recusado se escapar dela.
    Sem isso, um `.http` de origem não confiável poderia incluir
    `~/.ssh/id_rsa` e enviar o conteúdo para um servidor arbitrário com um
    clique. Inclusões bloqueadas ficam registradas no log e a linha `< ...`
    é mantida literal no corpo.
  - se sobrar algum `{{...}}` sem resolver, a requisição **não** é enviada: o
    resultado traz a lista das variáveis faltantes, em vez de um erro obscuro
    do cliente HTTP.
- O parser tolera os padrões comuns de arquivos reais: query string em várias
  linhas (linhas iniciadas por `?` ou `&`), comentários entre os cabeçalhos,
  parâmetros de query comentados e comentários depois do corpo (que não entram
  no corpo enviado).
- Com **vários `.http` abertos ao mesmo tempo**, todos mostram os lenses — e os
  botões continuam funcionando depois de editar os arquivos. Quatro defesas no
  servidor garantem isso, porque o cliente é a parte frágil aqui:
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
    até ser reaberta;
  - o clique não confia em nenhum argumento isolado do lens. O cliente **congela
    os argumentos do comando** quando recebe o lens e ancora só a posição na tela,
    e não os troca nem depois de receber os lenses de novo — medido: 104 lenses
    novos entregues a cada `didChange` e o clique seguinte ainda chegou com os
    argumentos antigos. O sintoma era o pior de todos: o botão simplesmente não
    surtia efeito, e só fechar e reabrir o `.http` resolvia. Por isso o lens leva
    **quatro pistas**, e `resolve_request` as testa da mais estável para a mais
    frágil, porque cada uma morre com um tipo diferente de edição:

    | pista | sobrevive a | morre com |
    | --- | --- | --- |
    | `# @name` | qualquer mudança na URL | renomear a requisição |
    | identidade (método + URL + nome) | deslocamento de linhas | qualquer edição no texto da requisição |
    | linha | edição *dentro* da requisição | inserir/remover linhas acima |
    | método | — | corrobora a linha |

    As duas primeiras versões erraram justamente aqui. A primeira mandava só a
    linha (`requisição na linha 1336 não encontrada`, com a requisição em 1330).
    A segunda deu à identidade prioridade sobre a linha — e aí ligar um
    `&page_size=100` na query multilinha passou a invalidar o botão para sempre:
    `requisição não encontrada em ...:163 (key=Some(10878242406106393856))`, com a
    linha 163 ainda **certa**. Nome antes de identidade antes de linha resolve os
    dois, e o método impede o único risco real de cair na linha (linhas que
    andaram fariam disparar a requisição errada — são chamadas de API de verdade).
    Sem nenhuma casar, o servidor **avisa** em vez de ficar mudo, e a mensagem
    manda fechar e reabrir o arquivo, porque o refresh não desfaz o congelamento;
  - o servidor só pede `codeLens/refresh` numa edição quando ela mexeu na
    **posição ou no nome** de alguma requisição (`lens_signature`). O refresh é
    global: ele invalida os lenses de *todos* os buffers, mas o Zed só re-pede os
    dos editores que considera visíveis — um `.http` escondido atrás da aba de
    resposta, ou em outro painel, ficava sem nenhum lens até ser reaberto. Pedir
    refresh a cada tecla digitada, como antes, fazia o "▶ Send request"
    desaparecer depois de um tempo de uso; digitar dentro de um corpo JSON agora
    não invalida nada.

  O log fica em `<dir-privado>/http-request-client-lsp-<nome-do-workspace>.log`
  — um por projeto, porque o Zed sobe um language server por projeto aberto e
  com um caminho fixo os logs se misturavam. Ele registra uma linha
  `-> codeLens <uri>: N lens, M enviando` por requisição de lens, útil para
  diferenciar "o servidor não respondeu" de "o cliente não pediu", e para ver
  se o `⏳` chegou a ser servido.
- O resultado é formatado como status line + cabeçalhos + corpo (pretty-print
  quando JSON) e escrito em `<dir-privado>/requests/<nome-do-workspace>.http`:

  ```
  HTTP/1.1 200 OK
  Date: Wed, 22 Jul 2026 19:49:13 GMT
  Content-Type: application/json; charset=utf-8
  ...

  {
    "message": "Welcome to api application."
  }
  ```

  O buffer é atualizado em disco e o file watcher do Zed recarrega — sem roubar
  o foco, o que permite deixá-lo num split ao lado do `.http`. Quando o cliente
  não tem o arquivo aberto, ele é mostrado com `window/showDocument`
  (`takeFocus: false`), que é idempotente: não duplica aba, não deixa o buffer
  sujo e reabre a aba se ela tiver sido fechada.

  O Zed **não implementa** `window/showDocument` (medido em 2026-08-03: responde
  `-32601 Unrecognized method`, e nem anuncia a capability), então hoje quem roda
  na prática é o fallback abaixo; o caminho preferido fica pronto para quando ele
  passar a implementar. Se o cliente recusar o `window/showDocument`, responder
  com erro **ou não responder** em 3 s, o servidor passa a usar o mecanismo
  anterior —
  `workspace/applyEdit` com um `CreateFile`, uma vez por sessão — e entrega a
  resposta daquela requisição por lá também. Esse caminho existe porque
  `applyEdit` era, até aqui, a única forma conhecida de fazer o Zed abrir uma aba
  a pedido do language server; ele tem duas contrapartidas que o `showDocument`
  não tem: o `didClose` espúrio do Zed (aba de preview, o mesmo arquivo em dois
  painéis) obriga a escolher entre **duplicar a aba** e **escrever a resposta num
  arquivo invisível**, e o buffer nasce sujo (daí a recomendação de autosave).

  O arquivo fica **fora do projeto**, com o nome do workspace — assim ele não
  suja o repositório nem precisa de `.gitignore`, e dois projetos abertos ao
  mesmo tempo não disputam o mesmo arquivo. Estar fora do worktree não
  atrapalha: testei, e o Zed cria um worktree invisível de arquivo único para
  ele, registra o language server nele e observa o arquivo normalmente — a
  escrita em disco gera `didChange` como antes. Consequência esperada de morar
  no diretório temporário: as respostas não sobrevivem a um boot.

  O `<dir-privado>` é
  `<temp>/http-request-client-<uid>/<nome-do-workspace>-<hash-da-raiz>/`, com
  permissão `0700` (e os arquivos com `0600`). Respostas de API costumam trazer
  tokens e dados sensíveis, e o diretório temporário é compartilhado: com um
  caminho fixo e permissão padrão, qualquer outro usuário (ou serviço) da máquina
  conseguiria **ler** as respostas, ou plantar um symlink no caminho previsível
  para **desviar** a escrita. O diretório de cima é criado em modo exclusivo e,
  se já existir, só é reaproveitado depois de conferir que é um diretório (não um
  symlink), com `0700` e do nosso uid — senão o servidor cai num nome aleatório.
  Como ninguém mais atravessa esse diretório, o subdiretório do workspace pode
  ter nome previsível. As URLs registradas no log também vão **sem query
  string**, que é onde tokens costumam viajar.

  O caminho é **estável**: depende do usuário e da raiz do workspace, não do
  processo. Ele já foi aleatório por processo, e isso era um bug — como o Zed
  derruba e sobe o language server ao longo da sessão, cada ciclo estreava um
  caminho de resultado, o Zed abria **mais uma aba de resposta** e as antigas
  ficavam órfãs (era assim que apareciam dezenas de `/tmp/http-request-client-*`
  numa tarde de uso).

### Limitação conhecida (destaque de sintaxe)

A gramática `tree-sitter-http` reconhece interpolações no formato
`{{identificador}}` (sem espaço) — cobre bem casos como
`{{login.response.body.json.key}}`. Já variáveis de processador com
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
# @name updateExample
PUT {{HOST}}/put
content-type: {{CONTENT_TYPE}}

{
  "name": "novo nome",
  "active": true
}

# active: liga/desliga o registro   <- comentário após o body: quebra o highlight
###
```

Workaround: colocar os comentários de documentação **antes** do body (junto aos
cabeçalhos, onde a gramática os aceita):

```http
# @name updateExample
# active: liga/desliga o registro   <- comentário antes do body: OK
PUT {{HOST}}/put
content-type: {{CONTENT_TYPE}}

{
  "name": "novo nome",
  "active": true
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
