# Política de segurança

Não publique vulnerabilidades ou arquivos de teste maliciosos em issues públicas.
Envie um relatório privado aos mantenedores com a versão afetada, passos para
reprodução e impacto. Não inclua credenciais ou dados pessoais.

O parser deve rejeitar magic, versões, offsets, tamanhos, caminhos e declarações
de recursos inválidos antes de alocar ou escrever. Toda correção de segurança deve
ter um teste de regressão e preservar a restauração byte-exact.

## Fronteira do daemon local

O `pithosd` usa a identidade do usuário local como fronteira de segurança. IPC,
capabilities e `path_scope` impedem acesso entre usuários e limitam requisições a
raízes canonicalizadas, incluindo rejeição de escapes por symlink e revalidação
antes da execução. Eles não isolam processos mutuamente hostis que já executem com
a mesma identidade e possam alterar concorrentemente o namespace do filesystem.
Nesse cenário, execute o daemon sob uma conta dedicada com permissões restritas às
raízes autorizadas.
