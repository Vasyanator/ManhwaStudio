# Janela **Novo Projeto**

**Observação:** as capturas de tela foram feitas com a interface em russo. Refazê-las em português é uma tarefa à espera de alguém voluntário — pull requests são bem-vindos.

![image](../images/Окно-Новый-проект/1.png)

Baixa o capítulo de vários sites e faz o pré-processamento.

## Processamento em lote
Download e processamento em massa de capítulos com base em um grafo de nós. Ainda inacabado e sem polimento. Funciona parcialmente. Não dê atenção.


## **Importação**
O botão `Abrir pasta` permite abrir uma pasta com as imagens da manhwa e importá-las.

- É possível abrir uma pasta com um capítulo já baixado; nesse caso as imagens devem estar nomeadas na ordem correta, por exemplo `1.png/jpg/jpeg`
- É possível abrir um site com o capítulo salvo no navegador comum. 
  - Nesse caso o programa examina o arquivo `html` que está um nível acima, com o nome da pasta, e carrega as imagens na mesma ordem em que estavam na página.
  - Se o arquivo HTML não for encontrado, o programa tentará carregar as imagens ou os `resource(X)` como imagens na ordem dos nomes.
  - É possível definir um padrão de nomes de arquivos, caso os arquivos de imagem tenham nomes fora do comum
- O filtro de ±50% de largura funciona bem com quadrinhos de formato vertical, ajudando a remover imagens de publicidade, mas **é melhor desativá-lo para mangá e outros quadrinhos em páginas**, senão podem sumir páginas

O botão `Abrir arquivo` permite abrir uma imagem avulsa, um arquivo compactado ou um arquivo html de um site baixado.

O botão `Colar da área de transferência` permite colar uma única imagem copiada.

`Modo de adição` - alterne para não apagar a tira por completo ao acrescentar uma imagem esquecida.

## **Baixador rápido**
![image](../images/Окно-Новый-проект/2.png)

- O campo de entrada no topo e o botão de download permitem baixar rapidamente um capítulo gratuito de comic.naver.com, **!Não de series.naver.com!**
- Passe o cursor sobre o botão de download para ver os sites compatíveis.


## **Baixador avançado**
![image](../images/Окно-Новый-проект/3_1.png)
![image](../images/Окно-Новый-проект/3_2.png)
![image](../images/Окно-Новый-проект/3_3.png)

Abre a página indicada em um navegador completo e baixa as imagens pelo método escolhido.

### **Interceptação profunda**
O modo mais simples e universal, que funciona até com sites complicados. Mas ele **funciona apenas com o CloakBrowser** e baixa da página tudo o que parece uma imagem. **Depois que ele terminar, abrirá uma janela, e será preciso desmarcar manualmente as imagens que não pertencem ao capítulo, por exemplo a publicidade.**

## **Baixar Canvas da página**
Sua funcionalidade já está embutida na interceptação profunda; pode nem mexer. Baixa as imagens quando elas são `<canvas>` e não `<img>`.

## **Busca de links por padrão**
Método mais limpo, porém mais chato, que não funciona em todo lugar. **SÃO NECESSÁRIAS NOÇÕES BÁSICAS DE COMO FUÇAR NO CÓDIGO DA PÁGINA**, guia no final desta wiki.

Busca links por um modelo de prefixo:
- `*` significa qualquer combinação de caracteres
- `?` significa qualquer caractere único
- É um prefixo, então o começo dele é o que importa. O final instável pode ser omitido.

Os prefixos podem ser salvos e carregados.

### Coleta de links
Ajuda se nem todas as imagens apareceram na página de uma vez. Por exemplo, o site as carrega aos poucos, ou é um leitor paginado.

**Nesse caso, inicie a coleta, percorra todo o capítulo e pare a coleta.**

### Threads de download
O download multithread é muito mais rápido, mas nem sempre funciona. Se for preciso obter as imagens usando a sessão do navegador em vez de uma requisição comum, infelizmente o download é de thread única.


## **Junção/Corte**
![image](../images/Окно-Новый-проект/4.png)

Junta todas as imagens em uma única tira e depois as divide de modo a não cortar em cima de texto e de desenho. **!Não usar para mangá!**, apenas para manhwa/manhua e outros quadrinhos em forma de tira longa.

### **Parâmetros da junção**
- `Quantidade de partes`: Em quantas partes dividir a tira. Se estiver vazio, é automático.
- `Hmax`: Em partes de qual altura (em pixels) cortar a tira na divisão automática.
- `Faixa branca`: Uma linha de quantos pixels verificar quanto à cor uniforme ao marcar os pontos de corte. Em termos mais simples: quão fina pode ser uma faixa de cor única para que ali seja possível cortar.
- `Tolerância de cor uniforme`: Quanto a cor dos pixels pode variar no ponto onde se pode cortar. Vale aumentar se for um shoujo com um monte de imagens bonitas.
- `search radius`: Quão longe, para os dois lados do ponto de corte previsto, será procurado um lugar adequado.

### **Modos de funcionamento**
- `Juntar a tira` - simplesmente junta em uma única tira longa e nada mais
- `Juntar e colocar linhas de corte` - Junta e marca os pontos de corte para controle manual. Mais sobre eles abaixo.
- `Juntar e cortar automaticamente` - Junta e corta imediatamente nos pontos ideais. Rápido, mas o controle manual é melhor.
- `Juntar apenas nos pontos irregulares` - Não corta, apenas cola a tira onde os cortes caíam em cima de desenho ou textura

### **Junção e corte manuais**
Depois de `Juntar e colocar linhas de corte`, ou da adição manual de uma linha de corte, aparece esta interface:
![image](../images/Окно-Новый-проект/4_5.png)
  - A **seta vermelha** marca a linha de corte na barra de rolagem
  - A **seta azul** marca um **corte já existente**
  - A **linha vermelha** é o futuro corte propriamente dito; ela pode ser movida e excluída
  - O **botão vermelho** `Cortar` no topo aplica todos os pontos de corte e remonta a tira

- A linha de corte pode ser adicionada pelo menu do clique dir.
- Também no menu do clique dir. é possível juntar a página atual com a seguinte e com a anterior

### **Outras ações com a página**
![image](../images/Окно-Новый-проект/4_6.png)

Este é o menu de ações no canto de cada página.
- As setas para cima e para baixo trocam a página atual de lugar com a seguinte ou a anterior
- O X a exclui
- É possível recortar a página manualmente


## **Cortar como capítulo**
![image](../images/Окно-Новый-проект/4_1.png)

Toma como base o capítulo escolhido e corta as imagens exatamente da mesma forma. É necessário para baixar versões alternativas para a ferramenta Carimbo.

Se houver diferença na altura total dos dois capítulos, abrirá uma janela:

![image](../images/Окно-Новый-проект/4_2.png)
![image](../images/Окно-Новый-проект/4_3.png)

Aqui é preciso verificar se as imagens coincidem. A imagem do capítulo baixado ficará semitransparente. É preciso ajustar a altura de modo que fique como na primeira imagem, e não como na segunda.

### **Depois disso é preciso salvar como versão alternativa do capítulo escolhido, informando um nome.**


## **Processamento de imagens (Waifu2x/Reline)**
![image](../images/Окно-Новый-проект/5.png)

## Waifu2x

IA antiquada, mas ainda funcional, para remoção de ruído e ampliação. Mais simples e mais rápida que o Reline

## Reline

IA moderna para remoção de ruído e ampliação. Tem muitos modelos diferentes, principalmente para mangá. 


## **Salvamento**
![image](../images/Окно-Новый-проект/6.png)

Salva a série processada na estrutura do projeto ou simplesmente na pasta escolhida (salvamento independente).

Se você está apenas salvando o primeiro capítulo, escolha "Salvar como base do projeto", informe o nome e clique em "Salvar e abrir".

- A série é ao mesmo tempo um campo de texto e uma lista suspensa. Você pode digitar o seu próprio.


# Fuçando o site e criando o prefixo
Usando o mto.to como exemplo

## 1. Abrimos o capítulo em um navegador comum e apertamos F12
![image](../images/Окно-Новый-проект/7.png)

## 2. Passamos o cursor sobre as diferentes tags HTML e o próprio navegador mostra pelo que elas respondem. Se a parte do site com a imagem do capítulo estiver destacada, abrimos a tag até chegar à própria imagem.
![image](../images/Окно-Новый-проект/8.png)

## 3. Abrimos a tag da imagem específica e olhamos qual link há ali.
![image](../images/Окно-Новый-проект/9.png)
### Por exemplo, aqui temos o link `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp` Abrimos ele em uma nova aba e confirmamos que é uma imagem.

### Em seguida, abrimos mais algumas tags de imagens e reunimos os links. Por exemplo, veja:
- `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp`
- `https://n25.mbuul.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472994_800_12860_1448870.webp`
- `https://n21.mbrtz.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473001_800_15000_1578696.webp`
- `https://n06.mbwww.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473003_800_15000_1167770.webp`

## 4. Olhamos os links com atenção e procuramos o que há em comum. Por exemplo, veja:
- Por exemplo, o subdomínio sempre começa com n
- Nos nomes dos sites sempre há mb
- A primeira seção é sempre /media
- O resto, por exemplo `mbch/a97/6921b1dc4b5d85970424179a`, pode mudar de série para série

## 5. Lembramos como funciona o meu modelo simplificado
- `*` significa qualquer combinação de caracteres
- `?` significa qualquer caractere único

## 6. Montamos o modelo de prefixo
- Pegamos o começo do link, neste caso `https://n06.mbwww.org/media/`
- Substituímos tudo o que varia por curingas, por exemplo em vez de `n06` teremos `n*` ou `n??`
- Adicionamos um * no final
- Fica algo assim: `https://n*.mb*.org/media/*`

## 7. Parabéns! `https://n*.mb*.org/media/*` já pode ser inserido como prefixo no baixador avançado
