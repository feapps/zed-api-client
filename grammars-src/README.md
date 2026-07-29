# `grammars-src/tree-sitter-http`

Fork de [rest-nvim/tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http)
(MIT) com dois patches que o upstream não tem. É este fork que o
`extension.toml` referencia em `[grammars.http]`.

Não confundir com `grammars/` (sem `-src`): aquela pasta é o *checkout* que o
Zed faz a partir daqui, junto com o `.wasm` compilado — é gerada e está no
`.gitignore`.

Cada patch é um commit, em cima de um commit "vendor" com a árvore intocada do
upstream no rev `db8b4398de90b6d0b6c780aba96aaa2cd8e9202c` — então o diff para
o upstream são exatamente esses dois commits, prontos para virar PRs lá.

## Patch 1 — comentários em query params multilinha

No upstream, `target_url` absorve toda linha de continuação indentada:

```js
target_url: ($) =>
    seq($._target_url_line, repeat(seq(NL, WS, $._target_url_line))),
```

Como uma query string multilinha é escrita exatamente assim, um parâmetro
comentado vira texto de URL:

```http
GET https://example.com/get
    ?page=1
    # &page_size=10     <- vira parte da (target_url), pintado como URL
    &sort=asc
```

Ou seja, comentado e ativo ficavam com a mesma cor. Pior: comentar na coluna 0
não é alternativa — quebra o parse da requisição inteira (`ERROR`), porque a
linha seguinte deixa de ser continuação válida da URL.

O patch adiciona um token `URL_COMMENT` (`#` ou `//` até o fim da linha, sem
consumir a quebra de linha, com precedência léxica acima de `COMMENT_PREFIX`) e
o aceita como alternativa a uma linha de continuação:

```js
target_url: ($) =>
    seq(
        $._target_url_line,
        repeat(
            seq(NL, WS, choice(alias(URL_COMMENT, $.comment), $._target_url_line)),
        ),
    ),
```

A linha comentada passa a ser um nó `(comment)` aninhado na `(target_url)`, e o
`highlights.scm` da extensão já pinta `(comment)` como comentário.

Escopo do patch:

- só vale para linhas de continuação **indentadas** — um `#` na própria linha da
  URL (fragmento, `…/get#secao`) continua fazendo parte da URL;
- o resto da query string depois da linha comentada continua sendo parseado
  normalmente;
- os 37 testes do corpus do upstream continuam passando, mais um novo
  (`Multiline query params with commented out lines` em `test/corpus/request.txt`).

## Patch 2 — TAB conta como espaço em branco

O patch 1 sozinho não resolvia arquivos indentados com **TAB**, que é o caso da
maioria dos `.http` reais. A raiz: TAB (U+0009) é `\p{Cc}`, não `\p{Z}`, e o
upstream tratava os dois lados do problema errado:

```js
const PUNCTUATION = /[^\n\r\p{Z}\p{L}\p{N}]/u;  // TAB casa: vira "texto"
const WS = /\p{Zs}+/u;                          // TAB não casa: não é espaço
```

Numa query string multilinha, o separador `seq(NL, WS, …)` de `target_url`
falhava em toda linha indentada com TAB. A recuperação de erro então
transformava cada linha numa `section` própria, com um `request` sem método —
e como esse `request` tem uma `target_url`, a linha comentada voltava a ser
pintada como URL. Ou seja, o sintoma do patch 1 reaparecia inteiro, só que por
outro caminho.

A correção lista o TAB explicitamente dos dois lados (o que também evita que os
dois tokens disputem o mesmo caractere):

```js
const PUNCTUATION = /[^\n\r\t\p{Z}\p{L}\p{N}]/u;
const WS = /[\t\p{Zs}]+/u;
```

Isso conserta a indentação com TAB no grammar inteiro, não só na URL:
cabeçalhos, `###`, `@nome = valor`, linhas em branco. Num arquivo real de 1769
linhas indentado com TAB, a contagem de `section` cai de **368 para 109** sem
nenhum erro novo (os 4 `ERROR` que sobram são a limitação conhecida de
`{{$dotenv NOME}}`, com espaço, que o grammar não reconhece como `variable`).

Mais um teste no corpus: `Multiline query params indented with tabs`.

## Mexer no grammar

Precisa do `tree-sitter-cli` 0.23 (o mesmo que o upstream usa — versões mais
novas geram um `parser.c` com outro layout):

```sh
cd grammars-src/tree-sitter-http
npx tree-sitter-cli@0.23 generate   # regenera src/parser.c
npx tree-sitter-cli@0.23 test       # roda o corpus
git commit -am "..."
```

Depois atualize o `rev` em `extension.toml` com o novo commit e apague
`grammars/` na raiz para o Zed refazer o checkout:

```sh
rm -rf grammars/
```

Recarregue a extensão no Zed (`zed: reload extensions`, ou reinstale a dev
extension).

O `Cargo.toml` aqui é o do upstream (bindings Rust do tree-sitter) e não faz
parte do workspace da extensão — rodar `cargo` de dentro desta pasta falha com
`current package believes it's in a workspace when it's not`. É esperado: o
grammar é gerado pelo `tree-sitter-cli` e compilado pelo clang do Zed, nunca
pelo cargo.

## Publicar

Enquanto o fork mora aqui, o `repository` no `extension.toml` é um caminho
absoluto desta máquina — funciona para dev extension, mas não para publicar.
Para publicar, empurre este diretório para um repositório público e troque só a
URL (o `rev` é o mesmo commit):

```sh
cd grammars-src/tree-sitter-http
git remote add origin git@github.com:einstein-adriano/tree-sitter-http.git
git push -u origin HEAD:main
```

```toml
[grammars.http]
repository = "https://github.com/einstein-adriano/tree-sitter-http"
rev = "66e8d6559e2f2386b43574b1c565ebcdc213b4fe"
```

Os dois patches são independentes entre si (o `git log` fica `vendor` →
`comentários em query params` → `TAB`), então dá para levá-los ao upstream em
PRs separados.
