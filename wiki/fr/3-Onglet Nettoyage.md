# **Onglet Nettoyage**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

![image](../images/Вкладка-Клининг/1.png)

Sert à nettoyer le texte original des pages de la bande dessinée.
Il possède pour l'instant 2 outils — `Peindre` et `Suppression IA`

## **Le nettoyage a disparu ?**
À partir de la version 2.7, la structure a changé. Le nettoyage est désormais stocké dans le dossier **projects/{série}/{chapitre}/clean_layers** et non plus dans **cleaned**. Copiez simplement les images dans le nouveau dossier.

## **Panneau supérieur**
- `Effacer le calque` — efface tout ce qui a été dessiné
- `Afficher le calque` — bascule la visibilité de la surcouche de dessin. Permet de voir ce qui vient d'être dessiné et ce qui se trouve déjà sur l'image d'origine.
- `Nettoyage rapide` — si une détection de texte a été faite ; description complète ci-dessous.
- `Enregistrer le nettoyage` — enregistre les images dans le dossier de nettoyage du projet

## **Outil Peindre**
![image](../images/Вкладка-Клининг/2.png)

Pinceau rapide avec pipette, gomme et rectangle. Convient pour masquer du texte sur un fond uniforme.

- Le curseur montre la taille de la zone de dessin et la couleur actuelle (ici `rouge`)
- `Clic gauche` — dessin normal
- `Clic droit` — pipette, prend la couleur sous le centre du curseur
- `Shift+clic gauche` — gomme
- `Shift+molette de la souris` — taille du pinceau
- `Ctrl+clic gauche` — sélection et remplissage d'une zone rectangulaire, pour un nettoyage encore plus rapide

## **Outil Suppression IA**
![image](../images/Вкладка-Клининг/3.png)
![image](../images/Вкладка-Клининг/4.png)

Sélectionne une zone de la bande et supprime les objets sous le masque. Utilise l'IA du dépôt `advimman/lama`

- Avec `Shift+clic gauche`, sélectionnez une zone de la bande
  - **Si la fenêtre ne s'est pas ouverte, c'est que la sélection contenait plus d'une page**
- Une nouvelle fenêtre s'ouvre pour dessiner le masque
  - Dessiner le masque au `clic gauche`
  - Effacer le masque au `clic droit`
  - Changer la taille du pinceau avec `Shift+molette de la souris`
- Le bouton `Traiter` lance l'IA et supprime l'objet sous le masque
- Vous pouvez activer `Refine`, cela donne parfois un résultat un peu meilleur
- Si quelque chose ne va pas, vous pouvez cliquer sur `Annuler` et redessiner le masque
- Vous pouvez sélectionner à nouveau avec le masque et supprimer les artefacts
- Le bouton `Fermer` referme simplement cette fenêtre
- Le bouton `Appliquer applique la zone modifiée sur la bande`

### Autres modèles d'IA

- `Lama MPE` — modèle Lama plus petit et un peu moins bon, issu du dépôt zyddnys/manga-image-translator. Mais il fonctionne parfois mieux avec le style anime et les bandes dessinées que le Lama ordinaire
- `AOT` — modèle vraiment petit, entraîné sur du manga. Également issu de zyddnys/manga-image-translator

## **Outil Dégradé**
![image](../images/Вкладка-Клининг/5.png)
![image](../images/Вкладка-Клининг/6.png)

Sélectionne une zone de la bande et tente de reconstituer le dégradé sous le masque. Il remplit souvent le dégradé mieux que l'IA

- Les commandes sont identiques à celles de l'outil `Suppression IA`
- Il ne casse rien si un morceau de couleur uniforme se retrouve sous le masque
- Le programme peut se figer un instant, c'est normal

## **Outil Tampon**
![image](../images/Вкладка-Клининг/8.png)
![image](../images/Вкладка-Клининг/8_1.png)

Prend la zone située sous lui au même endroit d'une autre image, et dessine avec. 

Permet de reprendre quelque chose d'une autre version de ce même chapitre, par exemple les onomatopées en anglais. Ou de supprimer des filigranes grâce à une version traduite dans n'importe quelle autre langue qui n'en a pas.

### **Pour utiliser cet outil, il faut télécharger et enregistrer dans le téléchargeur une version alternative de ce chapitre.**

Il possède les paramètres suivants :

- Source : dossier d'images. Il peut y en avoir plusieurs. Dans le dossier du projet, cela se trouve dans le dossier alt_vers.
- Taille : taille du pinceau. Se règle aussi avec **Shift+molette de la souris**
- Aperçu : règle l'opacité de l'aperçu à l'intérieur du cercle du pinceau.
- Décalage Y : décale de haut en bas l'image depuis laquelle la zone est dessinée. Utile si des bannières y ont été insérées.

Commandes :

- Clic gauche : dessiner
- Clic droit : gomme
- Shift+clic gauche : gomme (sélection rectangulaire)
- Ctrl+clic gauche : remplir (sélection rectangulaire)

## **Nettoyage rapide**
![image](../images/Вкладка-Клининг/7.png)
![image](../images/Вкладка-Клининг/7_1.png)

![image](../images/Вкладка-Клининг/7_2.png)

### **Faites d'abord la détection de texte dans l'onglet Traduction**

Utilise le masque de texte issu de sa détection dans l'onglet de traduction pour tenter de masquer le texte sur un fond uniforme.
Il ne peint que si la couleur est identique sur les bords du masque.

- `Élargissement automatique du masque` — de combien élargir le masque si la couleur s'est révélée non uniforme. Aide à nettoyer un peu plus de texte. Se déclenche 1 fois.


## **Comment faire le nettoyage dans Photoshop ?**
### Nettoyage complet
- Prenez les images dans **projects/{série}/{chapitre}/scr**
- Traitez-les dans Photoshop
- Enregistrez-les dans le dossier **projects/{série}/{chapitre}/clean_layers** et redémarrez le programme

### Traiter une zone difficile
- Sélectionnez la zone avec l'un des outils d'édition de zone (OpenCV/Dégradé/IA)
- Sans rien modifier, cliquez sur **Appliquer** : cette zone sera reportée sur le calque transparent de nettoyage
- Cliquez sur **Enregistrer les calques**
- Ouvrez dans Photoshop l'image correspondante depuis le dossier **projects/{série}/{chapitre}/clean_layers**
- Traitez-la, enregistrez-la et redémarrez le programme