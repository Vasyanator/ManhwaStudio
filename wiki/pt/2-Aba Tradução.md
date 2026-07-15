# Aba **Tradução**

**Observação:** as capturas de tela foram feitas com a interface em russo. Refazê-las em português é uma tarefa à espera de alguém voluntário — pull requests são bem-vindos.

### **Instruções de como traduzir no final**
![image](../images/Вкладка-Перевод/1.png)
Aqui é possível criar balões de texto, reconhecer texto e inserir a tradução.

## **Balão de texto**
![image](../images/Вкладка-Перевод/2.png)

Serve para a tradução inicial. Depois, permite inserir o texto rapidamente durante a diagramação.

- É criado manualmente com a tecla T e sai para a esquerda ou para a direita a partir do ponto da tira onde o cursor estava no momento da criação
- Pode ser criado por OCR; nesse caso ele conterá o texto reconhecido.
- Pode ser excluído com a tecla Del
- Pode ser copiado e colado com Ctrl+C e Ctrl+V; é possível duplicar com Ctrl+D
- Pode ser arrastado
- **Linha superior** - texto original
- **Linha inferior** - tradução. Diferentemente dos demais elementos, que existem apenas na aba de tradução, esta linha estará em todas as abas.
- O número, neste caso `42`, é o número da fala, para os casos em que a ordem das falas não é de cima para baixo. Por exemplo, no mangá. Será usado na composição da tradução.
- A linha após o número, neste caso `Apresentador`, é o nome do personagem que fala, ou outra descrição da fala, por exemplo `pensamentos do protagonista` ou `legenda`. A linha tem autocompletar, sugerindo nomes de personagens criados na aba correspondente ou outros modelos.
- Tem os seguintes botões:
  - `Traduzir`: Traduz a linha superior e insere o resultado na linha inferior. É usado o serviço selecionado no painel de tradução automática.
  - `Excluir:` Exclui o balão de texto

## **Balão com imagem**
![image](../images/Вкладка-Перевод/2_1.png)

### **Servem principalmente para a tradução por IA via API**
- São criados com `Q` ou por seleção com `Shift+Q`
- Podem conter um fragmento da página dentro da seleção vermelha, ou uma imagem externa
- `Descrição` é preenchida manualmente e explica à IA o que é aquilo
- `Original` e `Tradução` são preenchidos pela IA
- Podem ter várias áreas de texto de uma vez dentro da moldura vermelha.
  - Fora da aba de tradução, elas serão balões separados


## **Inserção rápida do nome do personagem**
![alt text](<../images/2-Вкладка Перевод/image-1.png>)
No topo da tira são exibidos os 6 últimos nomes usados.
Para inserir rapidamente um deles, segure o número desejado, de 2 a 6, junto com a tecla de atalho de criação de balão ou de seleção. Por exemplo, `4 + T` ou `Shift + 2 + clique esq.` 

## **Reconhecimento de texto**
![image](../images/Вкладка-Перевод/6.png)

- `Shift+clique esq.` seleciona a área da tira da série onde o texto será reconhecido

### EasyOCR
Motor mais simples e universal, com suporte a vários idiomas.

### PaddleOCR
OCR avançado feito por engenheiros chineses, bom para chinês, japonês, inglês e coreano. Mas pode não funcionar para todo mundo.

### MangaOCR
OCR apenas de japonês, treinado especificamente em mangá. Muitas vezes já leva em conta a leitura da direita para a esquerda nas colunas.

### Surya
O maior motor de reconhecimento de texto; não exige a escolha do idioma. Em alguns casos pode ser mais preciso, mas é o mais lento de todos.

### AI API
Peça ao ChatGPT ou ao DeepSeek para reconhecer um texto difícil. O método mais caro e mais preciso.

### PaddleOCR-VL
Algo intermediário entre o Surya e o PaddleOCR

### Configurações
- `Manter as quebras de linha` - é claro pelo nome
- `Copiar para a área de transferência` - se deve copiar o texto reconhecido para a área de transferência
- `Colunas da direita para a esquerda` - Útil ao trabalhar com mangá, onde o texto japonês frequentemente aparece em colunas lidas da direita para a esquerda. Se ativado, a ordem das linhas reconhecidas será invertida.
- `Criar balão` - se deve criar um balão com o texto reconhecido no centro da área selecionada
- `Substituir caracteres` - Configure manualmente o que substituir por quê. Por padrão, substitui pontos no meio da linha por pontos comuns, e reticências por três pontos separados


## **Composição da tradução**
![image](../images/Вкладка-Перевод/3.png)![image](../images/Вкладка-Перевод/3_1.png)

Simplifica a montagem das falas para a IA.


**Configurações do painel de composição**

- `Ordenação`:
  - `Por altura` - Quanto mais abaixo na tira estiver o balão de texto, mais tarde ele será inserido. O número da fala não importa. Normalmente para quadrinhos de formato vertical.
  - `Por número da fala` - Ignora a altura e olha para o número da fala.
- `Copiar` - copia a composição para a área de transferência
- `Atualizar` - Atualiza a composição, mas normalmente isso não é necessário, pois acontece ao abrir o painel.
- `Substituição de \n` - Por que substituir a quebra de linha nos balões. Normalmente é um espaço, mas alguém pode precisar delimitar as linhas explicitamente, por exemplo se o OCR devolver a ordem errada.
- `Envolver as falas em` - acho que já está claro
- `Prefixo da fala` - o que inserir antes de cada fala
- `Limite de caracteres` - Até quantos caracteres montar a composição. As falas do último personagem serão inseridas por completo, mesmo que o limite seja ultrapassado.
- `Usar nomes de personagens` - Se desativado, apenas junta as falas, envolvendo-as somente em crase.
- `Mesclar falas do mesmo personagem` - Se ativado, insere entre as falas de um mesmo personagem o parâmetro seguinte
- `Entre falas do mesmo personagem` - Por padrão, uma nova linha
- `Entre falas` - O que inserir entre as falas quando os personagens são diferentes. Por padrão, duas novas linhas.

### **MiniJinja**

Permite montar qualquer composição de falas. Dê à IA os parâmetros disponíveis do primeiro campo de texto, peça que escreva o modelo desejado e cole-o no segundo campo.


## **Detector de texto em massa**
![image](../images/Вкладка-Перевод/7.png) ![image](../images/Вкладка-Перевод/7_1.png)

Quase igual ao do BallonsTranslator. Encontra os blocos de texto e os destaca em azul, as linhas em verde, e a máscara para a limpeza em vermelho.
Tem os seguintes parâmetros:

- `Algoritmo`: O clássico dispara falsos positivos com mais frequência e não gera máscara. Já a IA, para alguns, pode demorar mais, mas frequentemente encontra o texto melhor e gera uma máscara que depois pode ser usada para uma limpeza rápida.
- `Mostrar os blocos encontrados` - ocultar ou mostrar o contorno verde das linhas e os blocos azuis
- `Mostrar a máscara` - ocultar ou mostrar a máscara vermelha do texto
- `Expansão do bloco` - quanto expandir cada linha verde para cada lado. Recomenda-se 5-10
- `Distância de combinação` - A que distância as linhas serão combinadas em blocos. Recomendado 5
- `Reconhecer` - Usar o motor de reconhecimento carregado para reconhecer o texto nas áreas destacadas pela moldura azul.


## **Tradução automática**
![image](../images/Вкладка-Перевод/8.png)

### **NÃO é recomendado usar para tradução pública.**
- Pode ser usada para você mesmo ler rapidamente
- Para tradução pública, use preferencialmente IAs como ChatGPT, Gemini, DeepSeek e outras, além do painel de composição.

## **AI API**
![image](../images/Вкладка-Перевод/8_1.png)

- Envia automaticamente o contexto da série e as falas para a IA selecionada
- Permite traduzir somente imagens
- A qualidade já é aceitável para tradução pública
  - Mas ainda assim a qualidade é melhor se você enviar manualmente as falas compostas ao Gemini e depois inseri-las; assim fica melhor para editar em paralelo

Por enquanto tem suporte apenas ao Google e ao Yandex; o Deepl ainda não funciona.

## **Painel de balões**
![image](../images/Вкладка-Перевод/5.png)

Permite buscar e editar a tradução rapidamente


## **Como traduzir**
- Você cola na IA a instrução da aba `Notas de tradução`
- Reconhece o texto original. O que não for reconhecido, por exemplo fontes muito tortas, é melhor deixar para depois, ou já dar um print para a IA.
- Indica quem fala onde
- Cola na IA as falas compostas
- Cola o texto traduzido pela IA no balão de texto no lugar certo, via `clique dir.` -> `Colar na tradução`
- Assim você traduz o texto principal
- Depois, separadamente, faz prints do texto torto/onomatopeias e traduz pela IA, criando novos balões com T

De modo geral, você pode traduzir como quiser, com seu próprio conhecimento do idioma ou com um tradutor comum, mas se não souber o idioma, é melhor usar a IA.
