; Métodos HTTP (GET, POST, PUT, DELETE, ...)
(method) @function.method

; Nome do cabeçalho (Authorization, content-type, ...)
(header
  name: (_) @constant)

; Declaração de variável: @NOME = valor
(variable_declaration
  name: (identifier) @variable)

; Conteúdo de interpolação: {{NOME}}
(variable
  name: (identifier) @variable)

; Operadores
(comment
  "=" @operator)
(variable_declaration
  "=" @operator)

; Metadados de comentário: # @name algumaCoisa
(comment
  "@" @keyword
  name: (_) @keyword)

; URL da requisição
(request
  url: (_) @string.special.url)

(http_version) @constant

; Resposta
(status_code) @number
(status_text) @string

; Pontuação
[
  "{{"
  "}}"
] @punctuation.bracket

(header
  ":" @punctuation.delimiter)

; Corpo externo (< @arquivo.json)
(external_body
  path: (_) @string.special.path)

; Comentários e separadores de requisição (###)
[
  (comment)
  (request_separator)
] @comment @spell
