# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · [Español](README.es.md) · [Français](README.fr.md) · **Português**

Um mini-estúdio simples para traduzir manhwa por conta própria. Serve bem para títulos pouco populares, sem limpeza complicada; títulos complexos é melhor deixar para equipes que trabalham com Photoshop.

> 📖 **Os recursos são descritos com muito mais detalhes na [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/pt).** A versão mais atualizada também está no próprio programa.

**Para sugestões, dúvidas, suporte e comunidade, convido você para o servidor do Discord ou para o Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Observação sobre as capturas de tela.** Todas as capturas abaixo foram feitas com a interface em russo. Substituí-las por capturas em português é uma tarefa à espera de alguém voluntário — pull requests são bem-vindos.

## Ideia principal: balões de texto ao lado da tira contínua de manhwa. Os balões indicam em que lugar fica o texto traduzido.

# Menu principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Janela de novo projeto com o baixador
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

Aqui ficam as opções de download e de processamento inicial. Você pode simplesmente abrir uma pasta, indicar um capítulo de um dos sites suportados, usar o navegador com modelos de links, ou abrir uma cópia offline da página.
Depois é possível costurar e fatiar a tira (para quadrinhos verticais) e remover ruído com o Waifu2x.

# Aba de tradução
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

É aqui que se faz a tradução e a edição. O texto pode ser reconhecido em vários idiomas via EasyOCR, PaddleOCR ou MangaOCR. Cada fala pode receber um papel, usado na composição do texto antes do envio para a IA. Ou então dá para apenas detectar o texto e passar tudo pela tradução automática, se você só quiser ler.

# Aba de limpeza
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

Aqui o texto original é coberto. Comparado ao Photoshop, os recursos são bem modestos, mas cobrir um fundo uniforme é bastante conveniente. Os modelos de IA para remover objetos sob uma máscara, ou a ferramenta de restauração de gradiente, dão uma qualidade bem razoável. Além disso, os trechos especialmente difíceis podem ser tratados à parte no Photoshop.

# Aba de texto
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />

É aqui que o texto traduzido é posicionado. Com Shift+botão esquerdo você seleciona a área destinada a ele e, se um bloco de texto lateral apontar para lá, o texto é inserido automaticamente. Também dá para digitá-lo manualmente.
O painel oferece diversos efeitos, de sombra e contorno até gradiente. As próprias imagens de texto podem ser recortadas com uma máscara de recorte ou transformadas em perspectiva. A diagramação é bem mais rápida do que no Photoshop.

# Aba de personagens
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Aqui você pode ver e editar os personagens do título. Seus nomes e descrições entram nas instruções para a tradução com IA, para melhorar a compreensão da história e padronizar os nomes.

# Aba de termos
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Igual aos personagens, só que sem imagens.

# Aba de notas de tradução
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

Aqui se escreve a instrução principal para a IA, e nela os personagens e os termos são inseridos automaticamente.
