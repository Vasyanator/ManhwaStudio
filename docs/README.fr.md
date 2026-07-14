# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · [Español](README.es.md) · **Français** · [Português](README.pt.md)

Un mini-studio simple pour traduire soi-même des manhwas. Il convient bien aux titres peu populaires qui ne demandent pas de nettoyage compliqué ; les titres complexes valent mieux être confiés à des équipes travaillant sous Photoshop.

Plus de détails dans le Wiki : https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki, mais la version à jour se trouve dans le programme lui-même.

**Pour les suggestions, les questions, le support et la communauté, rejoignez le serveur Discord ou Telegram** :
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Remarque sur les captures d'écran.** Toutes les captures ci-dessous sont prises avec l'interface en russe. Les remplacer par des captures en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

## Idée principale : des bulles de texte sur les côtés de la bande continue du manhwa. Les bulles indiquent l'endroit où se place le texte traduit.

# Menu principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Fenêtre de nouveau projet avec le téléchargeur
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

On y trouve les possibilités de téléchargement et de traitement initial. Vous pouvez simplement ouvrir un dossier, indiquer un chapitre issu des sites pris en charge, utiliser le navigateur avec des modèles de liens, ou ouvrir une copie hors ligne de la page.
Ensuite, vous pouvez assembler et découper la bande (pour les comics verticaux) et supprimer le bruit avec Waifu2x.

# Onglet de traduction
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

C'est ici que se font la traduction et l'édition. Le texte peut être reconnu dans de nombreuses langues via EasyOCR, PaddleOCR ou MangaOCR. Chaque réplique peut recevoir un rôle, utilisé lors de la mise en forme du texte avant l'envoi à l'IA. Vous pouvez aussi simplement détecter le texte et tout passer par la traduction automatique si vous voulez seulement lire.

# Onglet de nettoyage
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

C'est ici que le texte d'origine est recouvert. Comparées à Photoshop, les fonctions sont assez limitées, mais recouvrir un fond uniforme est très pratique. Les modèles d'IA de suppression d'objets sous un masque, ou l'outil de restauration de dégradé, donnent une qualité tout à fait correcte. De plus, les fragments particulièrement difficiles peuvent être traités séparément dans Photoshop.

# Onglet de texte
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />

C'est ici que le texte traduit est placé. Shift+clic gauche permet de sélectionner la zone qui lui est destinée, et si un bloc de texte latéral pointe vers cet endroit, le texte y est inséré automatiquement. Vous pouvez aussi le saisir à la main.
Le panneau propose divers effets, de l'ombre et du contour jusqu'aux dégradés. Les images de texte elles-mêmes peuvent être rognées avec un masque de rognage ou transformées en perspective. Le lettrage est nettement plus rapide que dans Photoshop.

# Onglet des personnages
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Vous pouvez y consulter et modifier les personnages du titre. Leurs noms et descriptions sont intégrés aux instructions de la traduction par IA, afin d'améliorer sa compréhension de l'histoire et d'uniformiser les noms.

# Onglet des termes
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Comme les personnages, mais sans images.

# Onglet des notes de traduction
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

C'est ici qu'on écrit l'instruction principale pour l'IA ; les personnages et les termes y sont insérés automatiquement.
