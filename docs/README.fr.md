# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · [Español](README.es.md) · **Français** · [Português](README.pt.md)

Un programme spécialisé pour traduire des bandes dessinées, y compris les mangas et les webtoons. Un outil tout-en-un pour le téléchargement, le prétraitement, la traduction, la suppression du texte d'origine et un lettrage avancé. Contrairement à ses alternatives, le programme est orienté vers le travail manuel plutôt que vers l'automatisation, et il est à bien des égards plus pratique et intuitif que Photoshop.

> 📖 **Les fonctionnalités sont décrites beaucoup plus en détail dans le [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/fr).** La version la plus à jour se trouve aussi dans le programme lui-même.

**Pour les suggestions, les questions, le support et la communauté, rejoignez le serveur Discord ou Telegram** :
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Pour installer, rendez-vous dans les [Releases](https://github.com/Vasyanator/ManhwaStudio/releases/latest), téléchargez et lancez l'exécutable pour votre système.** Windows, Linux et macOS sont pris en charge.

> **Remarque sur les captures d'écran.** Toutes les captures ci-dessous sont prises avec l'interface en russe. Les remplacer par des captures en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

## Idée principale : des bulles de texte sur les côtés de la bande continue de pages. Tout le chapitre est traité d'un coup. Les bulles indiquent l'endroit où se place le texte traduit.

# Menu principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Fenêtre de nouveau projet avec le téléchargeur
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

- Ouverture simple d'un dossier ou d'une archive contenant le chapitre
- Téléchargement rapide depuis les sites pris en charge
- Téléchargement depuis la plupart des sites en écrivant le bon préfixe des liens vers les images
- Téléchargement automatique de toutes les images avec sélection manuelle de celles qu'il faut
- Extraction des images de la plupart des sites en enregistrant la page et en ouvrant le HTML
- Recadrage lorsqu'il s'agit de captures d'écran
- Assemblage/découpage pour les webtoons. Un algorithme intelligent qui ne coupe pas au milieu du dessin
- Passage par Reline ou Waifu2x pour l'upscaling et la suppression du bruit

# Fenêtre d'import depuis PSD
<img width="2560" height="1356" alt="Image" src="https://github.com/user-attachments/assets/47602f13-1320-4d9e-ba71-b923c6d8b78f" />

- Ouverture d'un dossier ou d'une archive de fichiers PSD après le nettoyage
- Détection automatique de l'ordre des pages et séparation de l'original du calque nettoyé
- Import du nettoyage dans le chapitre

# Onglet de traduction
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

- Reconnaissance du texte via EasyOCR, MangaOCR, PaddleOCR, PaddleOCR-VL, SuryaOCR, API d'IA
- Création et édition des bulles de traduction avec attribution des personnages
- Création de bulles avec images pour la traduction par IA, par exemple pour les onomatopées
- Détection automatique du texte et traduction automatique, si vous voulez juste lire rapidement
- Traduction via les API de divers services d'IA
- Mise en forme des répliques pour pouvoir les exporter en docx ou les envoyer à une IA pour une traduction de meilleure qualité. L'envoi à l'IA nécessite l'attribution des personnages

# Onglet de nettoyage
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

- Pinceau rapide avec pipette et possibilité de peindre au rectangle
- Grille de pixels à fort zoom
- Suppression par IA du texte sur fond complexe à l'aide de différents modèles, de Lama à Flux
- Un algorithme de remplissage de dégradé qui fonctionne à merveille
- Synthèse de textures
- Nettoyage rapide du texte détecté sur fond uniforme

# Onglet de texte
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />
<img width="2556" height="1341" alt="Image" src="https://github.com/user-attachments/assets/65951a3f-386d-4c49-96d2-581cfee0acd4" />

La partie la plus avancée du programme.

- Sélection d'une zone avec Shift+clic gauche et insertion rapide du texte de la bulle tombée dans la sélection
  - La présence d'une bulle n'est pas obligatoire. On peut simplement coller une réplique copiée depuis un document contenant la traduction
- De nombreux paramètres de texte, avec modification individuelle de certains paramètres pour une partie du texte seulement (via la sélection)
- Déplacement, rotation, mise à l'échelle, déformation de l'image du texte, ligne de disposition du texte
- Masque de rognage du texte et son remplissage, permettant en deux clics de rogner le calque de texte quand il doit passer sous quelque chose
- Application de paramètres au texte tant qu'il est encore sous forme vectorielle. Par exemple rotation, étirement d'un caractère, gras/italique forcés
- Divers réglages et effets de texte, dont contour, flou, lueur, dégradé, reflet et tremblement

# Éditeur façon PS
<img width="2560" height="1343" alt="Image" src="https://github.com/user-attachments/assets/b5b5e83c-4ccb-4b1d-bb20-97a0e55218a8" />

Encore en développement, mais il permet déjà de travailler avec les calques :
- Couper
- Déplacer
- Découper en morceaux
- Dessiner

# Onglet des personnages
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Vous pouvez y consulter et modifier les personnages du titre. Leurs noms et descriptions sont intégrés aux instructions de la traduction par IA, afin d'améliorer sa compréhension de l'histoire et d'uniformiser les noms.

# Onglet des termes
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Comme les personnages, mais sans images.

# Onglet des notes de traduction
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

C'est ici qu'on écrit l'instruction principale pour l'IA ; les personnages et les termes y sont insérés automatiquement.
