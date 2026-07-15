# **Bande d'images :**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

![alt text](<../images/1-Лента картинок и её параметры/image.png>)
![alt text](<../images/1-Лента картинок и её параметры/image-1.png>)

Ici, toutes les pages sont affichées d'un coup. Pour les mangas et les bandes dessinées paginées, il y a un espacement entre les pages ; pour les webtoons, il n'y en a pas. Cela dit, mieux vaut ne pas couper une bulle en deux pages : pour cela, il y a l'assemblage et la découpe dans la fenêtre du nouveau projet.

# **Contrôle de la bande**
La bande se manipule comme un grand canevas : on peut la faire défiler, la déplacer, zoomer et créer des bulles directement sur la page.

### Défilement et déplacement
La molette de la souris fait défiler la bande vers le haut et vers le bas. Si la bande est plus large que la fenêtre à cause du zoom ou des bulles latérales, une barre de défilement horizontale apparaît en bas.

Pour déplacer la bande à la main, maintenez `Espace` et tirez avec la souris. C'est pratique à fort zoom, quand il faut décaler rapidement le canevas sur le côté.

En haut à gauche, il y a un petit panneau flottant. On y voit la page actuelle, l'échelle, l'interrupteur d'affichage des bulles et l'opacité des bulles. Le panneau peut être réduit et déplacé.

### Échelle
L'échelle change autour du point situé sous le curseur : en zoomant, la bande ne part donc pas ailleurs.

- `Ctrl` + molette de la souris — zoom avant ou arrière.
- `Z` + molette de la souris — la même chose, mais sans `Ctrl`.
- `Ctrl` + `+` / `Ctrl` + `-` — zoom avant ou arrière.
- `Ctrl` + `0` — réinitialiser le zoom.
- `Z` + `+` / `Z` + `-` / `Z` + `0` — les mêmes commandes de zoom via la touche `Z`.
- `Ctrl` ou `Z` + maintenir le clic gauche et déplacer vers la gauche/droite — zoom progressif par glissement.

Sur macOS, `Cmd` remplace généralement `Ctrl`.

### Création et sélection des bulles
Dans l'onglet `Traduction`, une nouvelle bulle peut être créée avec `T` à la position du curseur. La même action est disponible dans le menu contextuel de la page, au clic droit.

Un clic gauche sur une zone vide désélectionne la bulle. Un clic gauche sur une bulle la sélectionne. `Delete` supprime la bulle sélectionnée.

Le clic droit sur la page ouvre le menu de création et de collage des bulles. Le clic droit sur une bulle ouvre le menu de la bulle elle-même : on y trouve les actions de copie, de collage, de duplication, de changement de type, ainsi que des entrées supplémentaires si le correcteur orthographique est activé.

### Déplacement des bulles
Les bulles de type `Par-dessus` peuvent être déplacées directement sur la page et redimensionnées par les poignées des bords.

Les bulles de type `Latérale` se trouvent dans la colonne latérale, mais elles sont rattachées à un point de la page. On peut les faire glisser, et aussi déplacer la zone d'ancrage sur l'image elle-même. Une ligne montre à quel endroit se rapporte la bulle latérale.

### Annulation et rétablissement
Pour les actions sur les bulles, les raccourcis suivants fonctionnent :

- `Ctrl` + `Z` — annuler.
- `Ctrl` + `Shift` + `Z` — rétablir.
- `Ctrl` + `D` — dupliquer la bulle sélectionnée.

Tous ces raccourcis peuvent être réattribués dans les paramètres des raccourcis clavier.

# **Paramètres de la bande**
La bande possède de nombreux paramètres, ils se trouvent dans l'onglet correspondant > `Bande et bulles`.
![](<../images/1-Лента картинок и её параметры/image-2.png>)

### Préréglage des paramètres
Permet de mettre rapidement les paramètres standards pour les webtoons ou les bandes dessinées paginées.

- **Par pages** — les pages sont séparées par une marge, les bulles latérales dans l'onglet de traduction sont davantage compactées.
- **Webtoon** — les pages se suivent sans espace, les bulles latérales ne sont pas compactées.
- **Personnalisé** — s'active si les paramètres diffèrent des préréglages standards.

Cela ne modifie pas les images elles-mêmes, seulement le comportement de la bande et des bulles.

### Type de bulle par défaut dans l'onglet de traduction/nettoyage/texte
Détermine si les bulles de type standard s'affichent comme `Par-dessus` ou comme `Latérale`.

Dans l'onglet de traduction, cela concerne les nouvelles bulles et les bulles ordinaires sans type propre. Dans les onglets de nettoyage et de texte, cela concerne la façon dont les bulles standards déjà existantes sont affichées en mode consultation.

Si vous choisissez `Latérale`, la bulle sera dans la colonne latérale et reliée par une ligne à un endroit de la page. Si vous choisissez `Par-dessus`, la bulle sera posée directement sur la page.

### Insérer automatiquement le dernier personnage
Si activé, la création d'une nouvelle bulle y insère immédiatement le dernier personnage sélectionné. Pratique quand les répliques d'un même personnage se suivent.

### Vérifier l'orthographe de l'original / de la traduction
Active la mise en évidence orthographique dans les champs correspondants de la bulle. Pour la traduction, il est généralement utile de la laisser activée ; pour l'original, cela dépend : l'OCR produit souvent des noms, de l'argot et des morceaux d'une autre langue que le dictionnaire considérera de toute façon comme des erreurs.

### Mots personnalisés pour le correcteur orthographique
Ici, vous pouvez ajouter des mots qui ne doivent pas être signalés comme des erreurs.

- **Exclusions communes** fonctionnent pour tous les projets.
- **Exclusions du projet** ne sont enregistrées que pour le chapitre/projet actuel.

Écrivez un mot par ligne. C'est pratique pour les noms, les termes, les noms de techniques, les villes et les mots que le dictionnaire ne connaît pas.

### Étirer les bulles latérales
Gère la largeur des bulles placées à côté de la page.

Si activé, la largeur d'une bulle latérale s'adapte à la place libre à côté de la page, sans sortir des largeurs minimale et maximale. Autrement dit, la bulle essaie de ne pas dépasser de l'écran s'il y a de la place pour elle.

Si désactivé, les bulles latérales prennent toujours la largeur minimale.

### Réduire les bulles latérales dans l'onglet de traduction
Ce paramètre sert à éviter qu'une bande comportant beaucoup de bulles ne se transforme en une immense nappe d'interface.

- **Aucun** — la bulle est toujours entièrement déployée : original, traduction, boutons, numéro, personnage.
- **Modéré** — tant que la bulle n'est pas sélectionnée, seules les lignes de l'original et de la traduction sont visibles. Au focus, elle se déploie entièrement.
- **Fort** — tant que la bulle n'est pas sélectionnée, seule la ligne de traduction est visible. Si la traduction est vide, l'original est affiché. Au focus, elle se déploie entièrement.

Pour les webtoons, **Aucun** est souvent plus pratique, car les pages forment une bande continue. Pour les mangas paginés, **Fort** est souvent plus pratique, car les colonnes latérales prennent moins de place.

### Côté des bulles latérales
Détermine où afficher les bulles de type `Latérale`.

- **Auto** — la bulle apparaît à gauche ou à droite selon sa position sur la page.
- **À gauche** — toutes les bulles latérales vont à gauche.
- **À droite** — toutes les bulles latérales vont à droite.

En mode **Auto**, on peut déplacer l'ancre de la bulle sur la page, et le côté suivra sa position. Les modes forcés sont pratiques si l'on veut garder toute la traduction dans une seule colonne.

### Déploiement des bulles de type « Par-dessus »
Les bulles de type `Par-dessus` sont posées directement sur la page, dans leur rectangle de texte. Ce paramètre décide de ce qu'il advient de l'interface supplémentaire lorsqu'une telle bulle est sélectionnée.

- **Autour** — la bulle reste par-dessus la page, l'original s'affiche au-dessus, le personnage et les boutons en dessous.
- **Latérale** — la bulle sélectionnée se déploie temporairement comme une bulle latérale. C'est pratique quand on ne veut pas masquer le dessin avec des boutons et des champs.

### Taille des bulles latérales (%)
Met à l'échelle l'interface latérale : texte, boutons, marges et la colonne elle-même. 100 % — taille normale. En dessous de 100 %, les bulles latérales deviennent plus compactes ; au-dessus de 100 %, plus grandes.

### Largeur min. et max. des bulles latérales
Ce sont les limites de largeur de la colonne latérale.

- **Largeur min.** — une bulle latérale ne deviendra pas plus étroite que cela.
- **Largeur max.** — une bulle latérale ne s'étirera pas plus large que cela.

Si la largeur maximale devient par accident inférieure à la minimale, le programme l'aligne sur la minimale.

### Séparer les pages
Si activé, un espacement distinct apparaît entre les images. C'est le mode normal pour les mangas paginés et les bandes dessinées classiques.

Si désactivé, les pages se suivent sans interruption. C'est le mode webtoon, où tout le chapitre se lit comme une seule longue bande verticale.

### Espacement entre les pages
Ne fonctionne que si **Séparer les pages** est activé. Plus la valeur est grande, plus la distance entre les pages voisines est importante.

Pour les webtoons, ce paramètre est inutile, car les pages ne sont pas séparées.

### Marge haut/bas
Ajoute de l'espace vide au début et à la fin de la bande. Cela ne modifie pas les images elles-mêmes, cela donne juste une marge de défilement confortable pour que la première et la dernière page ne collent pas au bord de la fenêtre.

### Synchronisation automatique entre les onglets
Synchronise la position de la bande entre les onglets `Traduction`, `Nettoyage` et `Texte`. Si activé, vous pouvez passer à un autre onglet et rester à peu près au même endroit du chapitre.

Si vous le désactivez, chaque onglet vit avec son propre défilement.

### Mettre les pages en cache
Si activé, le programme garde à l'avance les pages décodées en mémoire pour des opérations rapides. Cela accélère le nettoyage, l'export et les autres actions qui ont besoin des pixels d'origine.

Si la mémoire est limitée ou si le chapitre est très gros, vous pouvez le désactiver. Le programme gardera alors moins de choses en mémoire, mais certaines actions pourront s'ouvrir plus lentement.

### Statut des bulles
Les statuts dessinent une bordure colorée autour des bulles selon des règles. Cela aide à voir rapidement quelles répliques ne sont pas encore prêtes.

Les règles s'appliquent de haut en bas : la première règle satisfaite choisit le style de la bordure. Une règle possède une condition et une bordure.

Les conditions se composent à partir de blocs :

- **Traduction remplie**
- **Original rempli**
- **Personnage rempli**
- **ET** — toutes les conditions imbriquées doivent être satisfaites
- **OU** — une seule condition imbriquée suffit
- **NON** — inverse la condition imbriquée

Pour la bordure, on peut choisir le type : continu, tirets, pointillés ou ondulé, ainsi que la couleur.

Le préréglage standard affiche une bordure rouge si la traduction n'est pas remplie, et une bordure verte si la traduction et le personnage sont remplis.
