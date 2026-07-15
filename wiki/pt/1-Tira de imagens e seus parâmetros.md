# **Tira de imagens:**

**Observação:** as capturas de tela foram feitas com a interface em russo. Refazê-las em português é uma tarefa à espera de alguém voluntário — pull requests são bem-vindos.

![alt text](<../images/1-Лента картинок и её параметры/image.png>)
![alt text](<../images/1-Лента картинок и её параметры/image-1.png>)

Aqui todas as páginas são exibidas de uma vez. Para mangá e quadrinhos em páginas existe um espaçamento entre as páginas; para webtoons não existe. Ainda assim, é melhor não cortar um balão em duas páginas — para isso existem a junção e o corte na janela de novo projeto.

# **Controle da tira**
A tira é controlada como uma grande tela: ela pode ser rolada, movida, ampliada, e é possível criar balões diretamente sobre a página.

### Rolagem e deslocamento
A roda do mouse rola a tira para cima e para baixo. Se a tira ficar mais larga que a janela por causa do zoom ou dos balões laterais, aparece uma barra de rolagem horizontal embaixo.

Para mover a tira com a mão, segure `Espaço` e arraste com o mouse. Isso é prático com muito zoom, quando você precisa deslocar a tela rapidamente para o lado.

No canto superior esquerdo há um pequeno painel flutuante. Nele são exibidos a página atual, o zoom, o interruptor de exibição dos balões e a opacidade dos balões. O painel pode ser recolhido e arrastado.

### Zoom
O zoom muda em torno do ponto sob o cursor, então ao aproximar a tira não salta para outro lugar.

- `Ctrl` + roda do mouse — aproximar ou afastar.
- `Z` + roda do mouse — o mesmo, mas sem `Ctrl`.
- `Ctrl` + `+` / `Ctrl` + `-` — aproximar ou afastar.
- `Ctrl` + `0` — redefinir o zoom.
- `Z` + `+` / `Z` + `-` / `Z` + `0` — os mesmos comandos de zoom através da tecla `Z`.
- `Ctrl` ou `Z` + segurar o botão esquerdo do mouse e mover para a esquerda/direita — zoom suave por arrasto.

No macOS, em vez de `Ctrl`, normalmente se usa `Cmd`.

### Criação e seleção de balões
Na aba `Tradução`, um novo balão pode ser criado com `T` na posição do cursor. A mesma ação está no menu de contexto da página, com o botão direito do mouse.

O clique esquerdo em um espaço vazio remove a seleção do balão. O clique esquerdo em um balão o seleciona. `Delete` exclui o balão selecionado.

O botão direito do mouse sobre a página abre o menu de criação e colagem de balões. O botão direito sobre um balão abre o menu do próprio balão: ali há ações de copiar, colar, duplicar, mudar de tipo, e itens adicionais se a ortografia estiver ativada.

### Deslocamento dos balões
Os balões do tipo `Sobreposto` podem ser movidos diretamente sobre a página e redimensionados pelas alças nas bordas.

Os balões do tipo `Lateral` ficam na coluna lateral, mas estão vinculados a um ponto na página. Eles podem ser arrastados, e também é possível mover a área de ancoragem sobre a própria imagem. Uma linha mostra a qual lugar o balão lateral se refere.

### Desfazer e refazer
Para as ações com balões funcionam:

- `Ctrl` + `Z` — desfazer.
- `Ctrl` + `Shift` + `Z` — refazer.
- `Ctrl` + `D` — duplicar o balão selecionado.

Todos esses atalhos podem ser reatribuídos nas configurações de atalhos de teclado.

# **Configurações da tira**
A tira tem muitas configurações; elas ficam na aba correspondente > `Tira e balões`.
![](<../images/1-Лента картинок и её параметры/image-2.png>)

### Predefinição de configurações
Permite aplicar rapidamente as configurações padrão para webtoons ou quadrinhos em páginas.

- **Em páginas** — as páginas são separadas por um espaçamento e os balões laterais na aba de tradução são reduzidos com mais intensidade.
- **Webtoon** — as páginas ficam coladas umas às outras e os balões laterais não são reduzidos.
- **Personalizado** — é ativado se os parâmetros diferirem das predefinições padrão.

Isso não altera as imagens em si, apenas o comportamento da tira e dos balões.

### Tipo de balão padrão na aba de tradução/limpeza/texto
Define se os balões do tipo padrão serão exibidos como `Sobreposto` ou `Lateral`.

Na aba de tradução isso afeta os balões novos e os balões comuns sem tipo próprio. Nas abas de limpeza e texto isso afeta como os balões padrão já existentes são mostrados no modo de visualização.

Se você escolher `Lateral`, o balão ficará na coluna lateral e se ligará ao ponto da página por uma linha. Se escolher `Sobreposto`, o balão ficará diretamente sobre a página.

### Inserir automaticamente o último personagem
Se ativado, ao criar um novo balão o último personagem selecionado é inserido nele imediatamente. É prático quando há falas seguidas do mesmo personagem.

### Verificar a ortografia no original / na tradução
Ativa o destaque ortográfico nos campos correspondentes do balão. Para a tradução costuma ser útil manter ativado; para o original, depende da situação: o OCR frequentemente produz nomes, gírias e trechos de outro idioma, que o dicionário considerará erros de qualquer forma.

### Palavras personalizadas para o corretor ortográfico
Aqui você pode adicionar palavras que não devem ser destacadas como erros.

- **Exclusões compartilhadas** funcionam para todos os projetos.
- **Exclusões do projeto** são salvas apenas para o capítulo/projeto atual.

Escreva uma palavra por linha. Isso é prático para nomes, termos, nomes de técnicas, cidades e palavras que o dicionário não conhece.

### Esticar os balões laterais
Responsável pela largura dos balões que ficam ao lado da página.

Se ativado, a largura do balão lateral se ajusta ao espaço livre ao lado da página, mas sem ultrapassar a largura mínima e a máxima. Ou seja, o balão tenta não sair da tela se houver espaço para ele.

Se desativado, os balões laterais sempre assumem a largura mínima.

### Reduzir os balões laterais na aba de tradução
Esta configuração serve para que uma tira com muitos balões não vire um enorme lençol de interface.

- **Nenhum** — o balão está sempre totalmente expandido: original, tradução, botões, número, personagem.
- **Moderado** — enquanto o balão não está selecionado, veem-se apenas as linhas do original e da tradução. Ao receber o foco, ele se expande por completo.
- **Forte** — enquanto o balão não está selecionado, vê-se apenas a linha da tradução. Se a tradução estiver vazia, o original é exibido. Ao receber o foco, ele se expande por completo.

Para webtoons costuma ser mais prático **Nenhum**, porque as páginas formam uma tira contínua. Para mangá em páginas costuma ser mais prático **Forte**, porque as colunas laterais ocupam menos espaço.

### Lado dos balões laterais
Define onde exibir os balões do tipo `Lateral`.

- **Auto** — o balão aparece à esquerda ou à direita conforme sua posição na página.
- **Esquerda** — todos os balões laterais ficam à esquerda.
- **Direita** — todos os balões laterais ficam à direita.

No modo **Auto**, é possível mover a âncora do balão na página, e o lado corresponderá à sua posição. Os modos forçados são práticos se você quiser manter toda a tradução em uma única coluna.

### Expansão dos balões do tipo "Sobreposto"
Os balões do tipo `Sobreposto` ficam diretamente sobre a página, dentro do seu retângulo de texto. A configuração decide o que fazer com a interface adicional quando esse balão está selecionado.

- **Ao redor** — o balão permanece sobre a página, o original é mostrado em cima, o personagem e os botões embaixo.
- **Lateral** — o balão selecionado se expande temporariamente como lateral. Isso é prático quando você não quer cobrir o desenho com botões e campos.

### Tamanho dos balões laterais (%)
Escala a interface lateral: texto, botões, margens e a própria coluna. 100% é o tamanho normal. Menos de 100% deixa os balões laterais mais compactos, mais de 100% os deixa maiores.

### Largura mín. e máx. dos balões laterais
Estes são os limites de largura da coluna lateral.

- **Largura mín.** — o balão lateral não ficará mais estreito que isso.
- **Largura máx.** — o balão lateral não se esticará além disso.

Se a largura máxima acabar ficando menor que a mínima, o programa a iguala à mínima.

### Separar páginas
Se ativado, aparece um intervalo próprio entre as imagens. Este é o modo normal para mangá em páginas e quadrinhos comuns.

Se desativado, as páginas ficam coladas umas às outras. Este é o modo webtoon, em que todo o capítulo é lido como uma única tira vertical longa.

### Espaçamento entre páginas
Funciona apenas quando **Separar páginas** está ativado. Quanto maior o valor, maior a distância entre páginas vizinhas.

Para webtoons este parâmetro não é necessário, porque as páginas não são separadas.

### Margem superior/inferior
Adiciona espaço vazio no início e no fim da tira. Isso não altera as imagens em si, apenas dá uma folga confortável para a rolagem, para que a primeira e a última página não fiquem grudadas na borda da janela.

### Sincronização automática entre abas
Sincroniza a posição da tira entre as abas `Tradução`, `Limpeza` e `Texto`. Se ativado, você pode passar para outra aba e permanecer aproximadamente no mesmo ponto do capítulo.

Se desativado, cada aba vive com a sua própria rolagem.

### Armazenar páginas em cache
Se ativado, o programa mantém antecipadamente as páginas decodificadas na memória para operações rápidas. Isso acelera a limpeza, a exportação e outras ações que precisam dos pixels originais.

Se houver pouca memória ou o capítulo for muito grande, você pode desativar. Nesse caso o programa manterá menos coisas na memória, mas algumas ações podem abrir mais devagar.

### Status dos balões
Os status desenham uma borda colorida ao redor dos balões conforme regras. Isso ajuda a ver rapidamente quais falas ainda não estão prontas.

As regras são aplicadas de cima para baixo: a primeira regra que coincide define o estilo da borda. Uma regra tem uma condição e um contorno.

As condições podem ser montadas a partir de blocos:

- **Tradução preenchida**
- **Original preenchido**
- **Personagem preenchido**
- **E** — todas as condições aninhadas devem coincidir
- **OU** — basta uma condição aninhada
- **NÃO** — inverte a condição aninhada

Para o contorno é possível escolher o tipo: sólida, tracejada, pontilhada ou ondulada, além da cor.

A predefinição padrão mostra uma borda vermelha se a tradução não estiver preenchida, e uma borda verde se a tradução e o personagem estiverem preenchidos.
