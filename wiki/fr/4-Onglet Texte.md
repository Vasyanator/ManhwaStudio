# **Onglet Texte**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

![image](../images/Вкладка-Текст/1.png)
Permet de placer des images de texte sur la bande

## **Principe de fonctionnement**
- Vous sélectionnez la zone du texte avec `Shift+clic gauche`
- Une fenêtre d'édition s'ouvre, dans laquelle est inséré le texte de la bulle qui se trouve dans la zone sélectionnée
- Après la perte du focus (un clic à côté), la fenêtre d'édition se ferme et une image de texte est créée avec les paramètres voulus
- L'image peut être déplacée par glissement 
- L'image peut être mise à l'échelle avec les touches `-` et `=`
- L'image peut être pivotée en survolant avec le curseur et `Ctrl+molette de la souris`
- On peut régler la taille de la police avec `Shift+molette de la souris`

## **Panneau des paramètres**
### **Aperçu du texte**
![image](../images/Вкладка-Текст/2.png)

Montre à quoi ressemblera le texte avec les paramètres actuels, sans tenir compte de la taille de la police. Se met à jour automatiquement.

### **Paramètres principaux du texte**
![image](../images/Вкладка-Текст/2_1.png)

- `Préréglage` : enregistrez et chargez les paramètres définis pour chaque police.
- `Police` : le programme est livré avec plusieurs polices, elles sont affichées dans la liste déroulante. Vous pouvez ajouter les vôtres en déposant un fichier ttf/otf dans le dossier fonts
- `face` : choix de la police dans une famille, s'il y en a plusieurs
- `Utiliser les polices système` : par défaut, les polices ne sont chargées que depuis le dossier fonts situé à côté de l'installation du programme
- `Groupe de polices` : limitez les polices affichées à celles qui se trouvent dans `fonts/groups/<nom du groupe>`. Les polices peuvent être dupliquées.
- `Taille` : taille de la police
- `Interligne` : espacement entre les lignes de texte, en pixels. Peut être négatif.
- `Crénage` : `Métrique` — distance toujours identique entre les lettres. `Auto` — utiliser les paires de caractères de la police, par exemple `AV`, pour que certains caractères soient plus proches. Ne fonctionne pas avec toutes les polices.
- `Crénage X` : distance supplémentaire entre les caractères
- `Hauteur/Largeur du caractère`
- `Alignement` : à gauche, à droite, centré ou libre. On peut déplacer le curseur pour une position intermédiaire.
- `Rotation globale` : fait pivoter tout le texte tant qu'il est encore à l'état vectoriel (non matricialisé). Donne une image nettement plus nette qu'une rotation ordinaire.
- `Forme` : principe de disposition des lignes, plus de détails ci-dessous.
- `Retour à la ligne` : césure automatique du texte pour respecter la forme. Règle son intensité ou la désactive.
- `Lissage` : comme le lissage dans Photoshop. Influe sur la netteté du texte.
- `Autoriser les chevrons modérés` : influe sur la forme du texte. Autorise des creux dans la forme.
- `Bold` : **police grasse**, ne fonctionne pas avec toutes les polices
- `Italic` : police inclinée, ne fonctionne pas avec toutes
- `Ponctuation suspendue` : les signes de ponctuation ne sont automatiquement pas pris en compte par l'alignement du texte. La liste des caractères se trouve dans les paramètres, mais ils y sont presque tous.
- `Supprimer les espaces superflus` : supprime les espaces en bordure de ligne.
- `Nouvelle ligne après la fin de phrase`
- `Tout en majuscules`
- `Analyser les balises` : permet de mettre une partie précise du texte dans `<b>` ou `<i>`, pour n'appliquer le style qu'à une partie du texte. Exemple : `Un <b>seul</b> mot sera en gras`. C'est aujourd'hui automatisé : sélectionnez simplement le texte, modifiez les paramètres disponibles, et il sera entouré de balises.

### **Paramètres avancés du texte**
![image](../images/Вкладка-Текст/2_2.png)

- `Ligne` : peut être horizontale (comme d'habitude) ou verticale.
  - Pour la verticale, on peut aller de droite à gauche comme de gauche à droite.
- `Disposition de la ligne` : standard ou formule. La formule permet de faire n'importe quelle forme.

### **Paramètres supplémentaires du texte**
Pas toujours disponibles.

#### **Paramètres pour le texte sélectionné uniquement**
- `Décalage X/Y` : décalage d'un caractère ou d'un groupe
- `Décalage le long de la ligne` : si le texte utilise une disposition personnalisée ou par formule, décale les caractères sélectionnés le long de celle-ci
  - `Décaler les caractères suivants` : uniquement pour le décalage le long de la ligne. Le texte suivant se déplacera avec la portion sélectionnée.
- `Rotation du caractère` : dans le texte sélectionné, fait pivoter chaque caractère séparément
- `Rotation du groupe` : fait pivoter tout le texte sélectionné ensemble
- `Ne pas couper` : le texte ne sera pas coupé par la césure automatique.


## **Panneau d'édition du texte**
![image](../images/Вкладка-Текст/13.png)


Apparaît lorsqu'une image de texte est sélectionnée. Ressemble au panneau de création de texte, mais possède un champ de texte. Les modifications s'appliquent immédiatement.

**Une partie du texte peut être sélectionnée, et modifier les paramètres disponibles créera automatiquement des balises en ligne.**


## **Forme du texte**
Il serait toutefois plus correct d'appeler cela une forme rapide.

Le texte essaie de tenir dans cette forme, en évitant les chevrons et en utilisant une césure intelligente.

Le texte peut avoir les formes suivantes :
- `Libre` : se place comme d'habitude.
- `[ ]` : carrée
- `( )` : ovale
- `< >` : hexagonale
- Les 2 dernières formes ont un paramètre de largeur minimale. Plus il est élevé, plus le haut et le bas du texte sont larges.

![image](../images/Вкладка-Текст/14.png)![image](../images/Вкладка-Текст/15.png)![image](../images/Вкладка-Текст/16.png)

Comme on le voit, les formes ne diffèrent pas beaucoup. Mais utilisez plutôt l'ovale.
Parfois, la force de la césure ne suffit pas à faire tenir le texte dans la forme. Jouez alors avec les paramètres de largeur, de taille de police, de largeur minimale, de force de la césure. Ou choisissez une forme rapide en faisant un clic droit sur le calque de texte.

## **Forme du texte avancée**
![image](../images/Вкладка-Текст/21_1.png)
![image](../images/Вкладка-Текст/21_2.png)

Se trouve dans le panneau d'édition.
Montre de quelques-unes à plusieurs centaines de milliers de formes possibles, sans chevrons et avec une césure correcte, que l'on peut filtrer.
Contrairement à la forme rapide, cette forme est stable et ne changera pas tant que vous ne la modifiez pas vous-même ou que vous n'en choisissez pas une autre. Elle est conservée lors d'un changement de police ou d'autres paramètres.

### Texte initial et texte formé
Lorsqu'une forme avancée est appliquée, le programme ne prend plus le texte initial pour créer le calque de texte, mais le texte formé. On peut basculer entre les deux. Le texte formé peut être corrigé soi-même et recevoir des balises. Mais si vous choisissez à nouveau une forme, le programme reprendra le texte initial pour la calculer, et en choisissant une autre forme, le texte formé sera écrasé.


## **Effets de texte**

### **Contour**
![image](../images/Вкладка-Текст/3.png)![image](../images/Вкладка-Текст/4.png)

Entoure le texte d'une ligne de la couleur et de l'épaisseur voulues

### **Lueur**
![image](../images/Вкладка-Текст/5.png)![image](../images/Вкладка-Текст/6.png)

Crée une aura autour du texte

### **Ombre**
![image](../images/Вкладка-Текст/7.png)![image](../images/Вкладка-Текст/8.png)

Ajoute au texte une ombre avec le décalage X et Y indiqué

### **Dégradé : 2 couleurs**
![image](../images/Вкладка-Текст/9.png)![image](../images/Вкладка-Текст/10.png)

Rend le texte dégradé dans la direction voulue

### **Dégradé : 4 coins**
![image](../images/Вкладка-Текст/11.png)![image](../images/Вкладка-Текст/12.png)

Ajoute au texte un dégradé basé sur quatre coins

### **Actions**
- `Actualiser l'image source` — recharge la page actuelle sur le canevas. Nécessaire si vous avez oublié de finir un nettoyage.
- `Afficher les bulles de texte` — possibilité de masquer les bulles de texte sur les côtés, pour mieux évaluer la traduction finale

### **Enregistrement**
Enregistre la série traduite de deux façons. Le rendu de scène est recommandé.


## **Masque de rognage**
![image](../images/Вкладка-Текст/17.png)![image](../images/Вкладка-Текст/18.png)
![image](../images/Вкладка-Текст/19.png)
![image](../images/Вкладка-Текст/20.png)

Permet de rogner les images de texte. Si une image de texte touche le masque, elle sera rognée, et seules les parties situées sous le masque resteront visibles. Pour chaque texte, le rognage peut être désactivé dans le menu du clic droit.
Il est jaune translucide et n'est pas visible quand ce panneau est fermé.

### **Pinceau du masque**

- Activé par défaut tant que le panneau est ouvert
- Dessin au `clic gauche`, gomme au `clic droit` ou `Shift+clic gauche`. La taille se règle avec `Ctrl+molette`.

### **Remplissage du masque**

- S'active en cliquant sur le bouton correspondant dans le panneau du masque
- Au clic gauche, il commence à remplir depuis le point cliqué, en se propageant sur les couleurs proches selon la tolérance


## **Transformation du texte (perspective)**
![image](../images/Вкладка-Текст/22.png)![image](../images/Вкладка-Текст/22_1.png)

On entre en mode transformation en faisant un clic droit sur le texte sélectionné et en choisissant l'entrée correspondante dans le menu.
Permet de déformer le texte, et pas seulement pour la perspective, en le tirant par ses poignées.


## **Disposition de texte personnalisée**
![image](../images/Вкладка-Текст/23_1.png)![image](../images/Вкладка-Текст/23_2.png)![image](../images/Вкладка-Текст/23_3.png)

Une chose utile pour les onomatopées.
**Pour commencer, choisissez dans le menu du clic droit du texte l'entrée « Entrer en mode d'édition de la disposition »**

**Ce mode ne permet pas de perdre le focus sur le texte : il faut cliquer volontairement sur « Quitter »**

- Changez la taille du texte en tirant la zone par les coins
- Après avoir sélectionné une ligne dans le panneau, faites un clic gauche dans la zone pour ajouter son début
- Avec Shift+clic gauche, faites glisser le point carré en laissant des points intermédiaires, pour définir la forme de la ligne
- Au clic gauche, faites simplement glisser les points, en changeant la forme sans en créer de nouveaux
  - Le grand point rond est le début de la ligne
  - Le petit point rond est un point intermédiaire
  - Le point carré est la fin, on peut étendre la ligne par celui-ci
  - Si les points et la ligne sont gris, c'est qu'une autre ligne est actuellement sélectionnée
    - Si tous les points et toutes les lignes sont gris, c'est que la ligne sélectionnée n'a pas encore été créée : faites un clic gauche pour créer le premier point.

### **Mécanique**
- À chaque ligne de cette disposition correspond un fragment de texte entre les sauts de ligne. On peut créer plusieurs onomatopées similaires à la fois

### **Panneau**
![image](../images/Вкладка-Текст/23_4.png)

- Ajout et suppression de lignes
- Lissage (rend la ligne moins anguleuse)
- Direction et retournement du texte
- Distance minimale au caractère : simplement le long de la ligne, ou en empêchant le chevauchement des caractères


## **Ajout de vos propres polices**
Dans le dossier du programme se trouve un dossier `fonts`, qui contient 4 polices principales — Anime Ace, une pour les légendes, une pour les onomatopées et Arial. Vous pouvez y déposer vos fichiers `.ttf`/`.otf`.


## **Disposition du texte par formule**
Je ne sais pas à quoi ça sert, mais ça existe.
La disposition par formule se trouve dans les `Paramètres avancés` :
- `Disposition` : `Standard` ou `Formule`
- la ligne des formules `x`, `y`, `rotation`
- le bouton `?` après les formules (afficher/masquer l'aide-mémoire des variables et des fonctions)
- les paramètres de trajectoire (`t_start/t_end`, `offset`, `scale`, `normal_offset`, `letter_spacing`)
- les constantes `a..h`

### **Comment ça marche**
Le programme calcule la position et l'angle **de chaque caractère séparément**.

Pour chaque caractère, on prend le paramètre `t` (généralement de `0` à `1`), puis :
- `x = formula_x(...)`
- `y = formula_y(...)`
- `rotation = formula_rotation(...)` (en radians)

Ensuite, les décalages/échelles sont appliqués et, si l'option est activée, la rotation selon la tangente à la trajectoire.

### **Paramètres du mode formule**
| Paramètre | Ce que ça fait | Comment l'utiliser |
|---|---|---|
| `Disposition` | Bascule le mode | `Standard` pour la disposition standard, `Formule` pour les arcs/spirales/trajectoires arbitraires |
| `x` | Formule de la coordonnée X pour chaque caractère | Point de départ de base : `t * w` |
| `y` | Formule de la coordonnée Y | Point de départ de base : `120 * sin((t - 0.5) * pi)` |
| `rotation` | Rotation supplémentaire de chaque caractère | `0` pour aucune rotation supplémentaire, `0.2*sin(2*pi*t)` pour une vague |
| `?` | Affiche/masque l'aide | Pratique quand il faut se rappeler rapidement les variables/fonctions |
| `Rotation tangentielle` | Fait pivoter le caractère le long de la direction de la courbe | Activez-la pour les arcs/spirales, désactivez-la pour un texte « droit » sur une courbe |
| `t_start` | Début de la plage du paramètre `t` | Généralement `0` |
| `t_end` | Fin de la plage de `t` | Généralement `1`, augmentez-le pour une « plus grande longueur » de courbe |
| `offset_x` | Décalage de toute la trajectoire selon X (px) | Déplacer toute l'inscription vers la droite/la gauche |
| `offset_y` | Décalage de toute la trajectoire selon Y (px) | Déplacer toute l'inscription vers le haut/le bas |
| `scale_x` | Échelle de la trajectoire selon X | `>1` étire, `<1` comprime |
| `scale_y` | Échelle de la trajectoire selon Y | Contrôle l'amplitude verticale |
| `normal_offset` | Décalage des caractères selon la normale à la courbe | Utile pour porter le texte vers l'extérieur/l'intérieur du cercle |
| `letter_spacing` | Multiplicateur de la distance entre les caractères | `1` normal, `>1` espace, `<1` resserre |
| `a..h` | Constantes utilisateur pour les formules | Gardez-y des « molettes » pour l'amplitude, le rayon, le nombre de tours, etc. Mais ce ne sont que des nombres, on ne peut pas y saisir de formule. |

### **Variables dans les formules**
- `t` — position actuelle dans la plage (`t_start..t_end`)
- `u` — position centrée (`-1..1`)
- `i` — index du caractère
- `n` — nombre de caractères
- `s` — longueur cumulée le long de la ligne (en pixels)
- `line` — index de la ligne
- `line_t` — position du caractère à l'intérieur de la ligne actuelle (`0..1`)
- `line_n` — nombre de caractères dans la ligne actuelle
- `w` / `width` — largeur du bloc de texte (px)
- `fs` / `font_size` — taille de la police
- `a..h` — vos constantes définies dans l'interface
- `pi`, `tau`, `math_e` — constantes mathématiques

### **Fonctions disponibles**
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `sqrt`, `abs`, `exp`, `ln`, `log`, `min`, `max`, `clamp`, `pow`, `rad`, `deg`, `floor`, `ceil`, `round`, `sign`

### **Important à propos de l'angle**
- `rotation` se définit en **radians**
- si les degrés sont plus pratiques, utilisez `rad(degrés)`, par exemple : `rad(25)`

### **Exemples tout prêts (copiez-les tels quels)**
#### 1) Arc
- `x` : `t * w`
- `y` : `120 * sin((t - 0.5) * pi)`
- `rotation` : `0`
- `Rotation tangentielle` : `activée`

#### 2) Ligne inclinée
- `x` : `t * w`
- `y` : `0.35 * t * w`
- `rotation` : `0`
- `Rotation tangentielle` : `désactivée`

#### 3) Vague
- `x` : `t * w`
- `y` : `80 * sin(2 * pi * t)`
- `rotation` : `0.15 * sin(2 * pi * t)`
- `Rotation tangentielle` : `désactivée`

#### 4) Spirale (via `a`, `b`, `c`)
- `a = 40`, `b = 180`, `c = 3`
- `x` : `(a + b * t) * cos(c * tau * t)`
- `y` : `(a + b * t) * sin(c * tau * t)`
- `rotation` : `0`
- `Rotation tangentielle` : `activée`

#### 5) Exponentielle
- `a = 3`
- `x` : `t * w`
- `y` : `140 * (exp(a * t) - 1) / (exp(a) - 1)`
- `rotation` : `0`
- `Rotation tangentielle` : `activée`

### **Démarrage rapide (pour ne pas casser la composition)**
Mettez les valeurs de base :
- `t_start = 0`
- `t_end = 1`
- `offset_x = 0`
- `offset_y = 0`
- `scale_x = 1`
- `scale_y = 1`
- `normal_offset = 0`
- `letter_spacing = 1`

Et ne modifiez qu'ensuite les paramètres, un par un.