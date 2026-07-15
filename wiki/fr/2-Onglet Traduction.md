# Onglet **Traduction**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

### **Instructions de traduction à la fin**
![image](../images/Вкладка-Перевод/1.png)
Ici, on peut créer des bulles de texte, reconnaître le texte et insérer la traduction.

## **Bulle de texte**
![image](../images/Вкладка-Перевод/2.png)

Sert à la traduction initiale. Elle permet ensuite d'insérer rapidement le texte lors du lettrage.

- Se crée manuellement avec la touche T et part vers la gauche ou vers la droite depuis le point de la bande où se trouvait le curseur au moment de la création
- Peut être créée par OCR, auquel cas elle contiendra le texte reconnu.
- Peut être supprimée avec la touche Suppr
- Peut être copiée et collée avec Ctrl+C et Ctrl+V, et dupliquée avec Ctrl+D
- Peut être déplacée par glissement
- **Ligne du haut** — le texte original
- **Ligne du bas** — la traduction. Contrairement aux autres éléments, qui ne sont présents que dans l'onglet de traduction, cette ligne sera dans tous les onglets.
- Le numéro, ici `42`, est le numéro de la réplique, pour les cas où l'ordre des répliques n'est pas de haut en bas. Par exemple pour le manga. Il sera utilisé dans la composition de la traduction.
- La ligne qui suit le numéro, ici `Ведущий`, est le nom du personnage qui parle, ou une autre description de la réplique, par exemple `pensées du héros`, ou `légende`. Cette ligne possède une autocomplétion qui propose les noms des personnages créés dans l'onglet correspondant, ou d'autres modèles.
- Elle possède les boutons suivants :
  - `Traduire` : traduit la ligne du haut et insère le résultat dans la ligne du bas. Le service choisi dans le panneau de traduction automatique est utilisé.
  - `Supprimer :` supprime la bulle de texte

## **Bulle avec image**
![image](../images/Вкладка-Перевод/2_1.png)

### **Surtout utiles pour la traduction par IA via API**
- Se créent avec `Q` ou par sélection `Shift+Q`
- Peuvent contenir un fragment de page à l'intérieur de la sélection rouge, ou une image externe
- La `Description` se remplit manuellement et explique à l'IA de quoi il s'agit
- L'`Original` et la `Traduction` sont remplis par l'IA
- Elles peuvent contenir plusieurs zones de texte à la fois à l'intérieur du cadre rouge.
  - En dehors de l'onglet de traduction, ce seront des bulles distinctes


## **Insertion rapide du nom d'un personnage**
![alt text](<../images/2-Вкладка Перевод/image-1.png>)
En haut de la bande sont affichés les 6 derniers noms utilisés.
Pour en insérer un rapidement, maintenez le chiffre voulu, de 2 à 6, en même temps que le raccourci de création de bulle ou de sélection. Par exemple `4 + T` ou `Shift + 2 + clic gauche` 

## **Reconnaissance de texte**
![image](../images/Вкладка-Перевод/6.png)

- `Shift+clic gauche` sélectionne la zone de la bande de la série où le texte sera reconnu

### EasyOCR
Moteur plus simple et polyvalent, il prend en charge de nombreuses langues.

### PaddleOCR
OCR avancé conçu par des ingénieurs chinois, bon pour le chinois, le japonais, l'anglais et le coréen. Mais il peut ne pas démarrer chez tout le monde.

### MangaOCR
OCR japonais uniquement, spécialement entraîné sur le manga. Il tient souvent compte d'emblée de la lecture de droite à gauche pour les colonnes.

### Surya
Le plus gros moteur de reconnaissance de texte, il ne demande pas de choisir la langue. Il peut être plus précis par endroits, mais il est le plus lent de tous.

### AI API
Demandez à ChatGPT ou DeepSeek de reconnaître un texte difficile. La méthode la plus coûteuse et la plus précise.

### PaddleOCR-VL
Quelque chose entre Surya et PaddleOCR

### Paramètres
- `Conserver les sauts de ligne` — le nom parle de lui-même
- `Copier le texte obtenu dans le presse-papiers` — faut-il copier le texte reconnu dans le presse-papiers
- `Colonnes de droite à gauche` — utile pour travailler sur du manga, où le texte japonais se présente souvent en colonnes qui se lisent de droite à gauche. Si vous l'activez, l'ordre des lignes reconnues sera inversé.
- `Créer une bulle` — faut-il créer une bulle contenant le texte reconnu au centre de la zone sélectionnée
- `Remplacer des caractères` — configurez manuellement quoi remplacer par quoi. Par défaut, les points au milieu de la ligne sont remplacés par des points ordinaires, et les points de suspension par trois points séparés


## **Composition de la traduction**
![image](../images/Вкладка-Перевод/3.png)![image](../images/Вкладка-Перевод/3_1.png)

Simplifie la mise en forme des répliques pour l'IA.


**Paramètres du panneau de composition**

- `Tri` :
  - `Par hauteur` — plus une bulle de texte est basse dans la bande, plus elle sera insérée tard. Le numéro de réplique n'a pas d'importance. Généralement pour les bandes dessinées au format vertical.
  - `Par numéro de réplique` — ignore la hauteur et se base sur le numéro de réplique.
- `Copier` — copie la composition dans le presse-papiers
- `Actualiser` — met à jour la composition, mais ce n'est en général pas nécessaire, car cela se fait à l'ouverture du panneau.
- `Remplacement de \n` — par quoi remplacer le saut de ligne dans les bulles. C'est généralement une espace, mais quelqu'un pourrait vouloir délimiter explicitement les lignes, par exemple si l'OCR donne un ordre incorrect.
- `Entourer les répliques` — je pense que c'est déjà clair
- `Préfixe de réplique` — ce qu'il faut insérer avant chaque réplique
- `Limite de caractères` — jusqu'à combien de caractères composer. Les répliques du dernier personnage seront insérées en entier, même si la limite est dépassée.
- `Utiliser les noms des personnages` — si désactivé, il assemblera simplement les répliques en les entourant seulement d'apostrophes inversées.
- `Fusionner les répliques du même personnage` — si activé, insérer le paramètre suivant entre les répliques d'un même personnage
- `Entre les répliques du même personnage` — par défaut, un saut de ligne
- `Entre les répliques` — ce qu'il faut insérer entre les répliques quand les personnages sont différents. Par défaut, deux sauts de ligne.

### **MiniJinja**

Permet de réaliser n'importe quelle composition de répliques. Donnez à l'IA les paramètres disponibles du premier champ de texte, demandez-lui d'écrire le modèle voulu, et collez-le dans le second champ.


## **Détecteur de texte en masse**
![image](../images/Вкладка-Перевод/7.png) ![image](../images/Вкладка-Перевод/7_1.png)

Presque comme celui de BallonsTranslator. Il trouve les blocs de texte et les entoure en bleu, les lignes en vert, et le masque pour le nettoyage en rouge.
Il possède les paramètres suivants :

- `Algorithme` : le classique se déclenche plus souvent à tort et ne génère pas de masque. L'IA, elle, peut être plus lente chez certains, mais elle trouve souvent mieux le texte et génère un masque qui pourra ensuite servir à un nettoyage rapide.
- `Afficher les blocs détectés` — masquer ou afficher le contour vert des lignes et les blocs bleus
- `Afficher le masque` — masquer ou afficher le masque rouge du texte
- `Élargissement du bloc` — de combien élargir chaque ligne verte dans chaque direction. 5-10 est recommandé
- `Distance de fusion` — à quelle distance les lignes seront fusionnées en blocs. 5 est recommandé
- `Reconnaître` — utiliser le moteur de reconnaissance chargé pour reconnaître le texte dans les zones entourées d'un cadre bleu.


## **Traduction automatique**
![image](../images/Вкладка-Перевод/8.png)

### **Il est fortement DÉCONSEILLÉ de l'utiliser pour une traduction publiée.**
- Vous pouvez l'utiliser pour lire rapidement pour vous-même
- Pour une traduction publiée, utilisez plutôt des IA comme ChatGPT, Gemini, DeepSeek et autres, ainsi que le panneau de composition.

## **AI API**
![image](../images/Вкладка-Перевод/8_1.png)

- Envoie automatiquement le contexte de la série et les répliques à l'IA choisie
- Permet de traduire uniquement les images
- La qualité est déjà acceptable pour une traduction publiée
  - Mais la qualité reste meilleure si l'on envoie manuellement les répliques composées à Gemini pour les coller ensuite : cela permet mieux d'éditer en parallèle

Pour l'instant, seuls Google et Yandex sont pris en charge, Deepl ne fonctionne pas encore.

## **Panneau des bulles**
![image](../images/Вкладка-Перевод/5.png)

Permet de rechercher et d'éditer rapidement la traduction


## **Comment traduire**
- Vous collez l'instruction pour l'IA depuis l'onglet `Notes de traduction`
- Vous reconnaissez le texte original. Ce qui n'est pas reconnu, par exemple les polices très tordues, vaut mieux être laissé pour plus tard, ou donné directement à l'IA sous forme de capture d'écran.
- Vous indiquez qui parle et où
- Vous collez les répliques composées dans l'IA
- Vous collez le texte traduit par l'IA dans la bulle de texte au bon endroit via `clic droit` -> `Coller dans la traduction`
- Vous traduisez ainsi le texte principal
- Ensuite, vous faites séparément des captures d'écran du texte tordu / des onomatopées et vous les traduisez via l'IA, en créant de nouvelles bulles avec T

Globalement, vous pouvez traduire comme vous voulez, avec votre connaissance de la langue ou un traducteur classique, mais si vous ne connaissez pas la langue, mieux vaut utiliser l'IA.