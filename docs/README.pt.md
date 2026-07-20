# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · [Español](README.es.md) · [Français](README.fr.md) · **Português**

Um programa especializado para traduzir quadrinhos, incluindo mangás e webtoons. Uma ferramenta tudo-em-um para baixar, pré-processar, traduzir, remover o texto original e fazer uma diagramação avançada. Diferentemente das alternativas, o programa é voltado ao trabalho manual, e não à automação, e em muitos aspectos é mais conveniente e intuitivo do que o Photoshop.

> 📖 **Os recursos são descritos com muito mais detalhes na [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/pt).** A versão mais atualizada também está no próprio programa.

**Para sugestões, dúvidas, suporte e comunidade, convido você para o servidor do Discord ou para o Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Para instalar, vá em [Releases](https://github.com/Vasyanator/ManhwaStudio/releases/latest), baixe e execute o arquivo executável para o seu sistema.** Windows, Linux e macOS são suportados.

> **Observação sobre as capturas de tela.** Todas as capturas abaixo foram feitas com a interface em russo. Substituí-las por capturas em português é uma tarefa à espera de alguém voluntário — pull requests são bem-vindos.

## Ideia principal: balões de texto ao lado da tira contínua de páginas. O capítulo inteiro é processado de uma vez. Os balões indicam em que lugar fica o texto traduzido.

# Menu principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Janela de novo projeto com o baixador
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

- Simplesmente abrir uma pasta ou um arquivo compactado com o capítulo
- Download rápido dos sites suportados
- Download da maioria dos sites escrevendo o prefixo correto dos links das imagens
- Download automático de todas as imagens com seleção manual das necessárias
- Extração de imagens da maioria dos sites salvando a página e abrindo o HTML
- Recorte quando são capturas de tela
- Costura/fatiamento para webtoons. Um algoritmo inteligente que não corta onde há desenho
- Processamento com Reline ou Waifu2x para upscale e remoção de ruído

# Janela de importação de PSD
<img width="2560" height="1356" alt="Image" src="https://github.com/user-attachments/assets/47602f13-1320-4d9e-ba71-b923c6d8b78f" />

- Abrir uma pasta ou um arquivo compactado com arquivos PSD após a limpeza
- Detecção automática da ordem das páginas e separação do original da camada limpa
- Importação da limpeza para o capítulo

# Aba de tradução
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

- Reconhecimento de texto via EasyOCR, MangaOCR, PaddleOCR, PaddleOCR-VL, SuryaOCR, API de IA
- Criação e edição de balões de tradução com indicação de personagens
- Criação de balões com imagens para tradução via IA, por exemplo para onomatopeias
- Detecção automática de texto e tradução automática, se você só quiser ler rapidinho
- Tradução através das APIs de diversos serviços de IA
- Composição das falas para poder exportá-las em docx ou enviá-las a uma IA para uma tradução de melhor qualidade. Para o envio à IA é preciso indicar os personagens

# Aba de limpeza
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

- Pincel rápido com conta-gotas e possibilidade de pintar com retângulo
- Grade de pixels em zoom alto
- Remoção por IA de texto sobre fundos complexos com diversos modelos, de Lama a Flux
- Um algoritmo de preenchimento de gradiente que funciona muito bem
- Síntese de texturas
- Limpeza rápida do texto detectado sobre fundo uniforme

# Aba de texto
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />
<img width="2556" height="1341" alt="Image" src="https://github.com/user-attachments/assets/65951a3f-386d-4c49-96d2-581cfee0acd4" />

A parte mais avançada do programa.

- Seleção de uma área com Shift+botão esquerdo e inserção rápida do texto do balão que cair na seleção
  - Não é obrigatório haver um balão. Dá para simplesmente colar uma fala copiada de um documento com a tradução
- Muitos parâmetros de texto, com alteração individual de alguns parâmetros apenas para parte do texto (via seleção)
- Mover, girar, escalar e deformar a imagem do texto, linha de disposição do texto
- Máscara de recorte do texto e seu preenchimento, permitindo recortar a camada de texto em poucos cliques quando ela deve ficar embaixo de algo
- Aplicação de parâmetros ao texto enquanto ele ainda está em forma vetorial. Por exemplo rotação, esticamento de um caractere, negrito/itálico forçados
- Diversos ajustes e efeitos de texto, incluindo contorno, desfoque, brilho, gradiente, reflexo e tremor

# Editor estilo PS
<img width="2560" height="1343" alt="Image" src="https://github.com/user-attachments/assets/b5b5e83c-4ccb-4b1d-bb20-97a0e55218a8" />

Ainda em desenvolvimento, mas já permite trabalhar com camadas:
- Cortar
- Mover
- Fatiar em partes
- Desenhar

# Aba de personagens
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Aqui você pode ver e editar os personagens do título. Seus nomes e descrições entram nas instruções para a tradução com IA, para melhorar a compreensão da história e padronizar os nomes.

# Aba de termos
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Igual aos personagens, só que sem imagens.

# Aba de notas de tradução
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

Aqui se escreve a instrução principal para a IA, e nela os personagens e os termos são inseridos automaticamente.
