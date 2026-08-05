# Corpus

O arquivo `corpus.schema.json` define o manifesto versionado usado por benchmarks,
vetores de teste e comparações de codecs. Cada entrada deve declarar origem,
licença e, quando o artefato estiver disponível, o SHA-256 para permitir reprodução.

O corpus não é armazenado automaticamente no repositório. Artefatos grandes,
restritos ou sem licença compatível devem permanecer fora dele e ser referenciados
por uma URL controlada.
