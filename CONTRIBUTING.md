# Współtworzenie

## Nazwy branchy

Format:

```
<typ>/<numer-issue>-<nazwa>
```

- `<typ>` — rodzaj zmiany, te same typy co w commitach:
  - `feat` — nowa funkcja,
  - `fix` — poprawka błędu,
  - `chore` — porządki, zależności, konfiguracja,
  - `test` — testy,
  - `docs` — dokumentacja,
  - `refactor` — zmiana kodu bez zmiany działania,
  - `ci` — GitHub Actions,
  - `build` — budowanie i instalatory.
- `<numer-issue>` — numer issue na GitHubie, bez `#`. Jeśli zmiana nie ma
  issue, pomiń numer: `<typ>/<nazwa>`.
- `<nazwa>` — tytuł issue albo jego skrót: małe litery, bez polskich znaków,
  słowa rozdzielone myślnikami.

Przykłady:

```
feat/1-wysylanie-plikow
fix/12-echo-w-rozmowie
chore/aktualizacja-zaleznosci
test/7-test-czatu
```

## Licencja wkładu

BibiCord jest udostępniany na licencji MIT (`LICENSE`). Wysyłając zmiany do
projektu (pull request, commit, łatka), zgadzasz się na udostępnienie ich na
tej licencji.

Wysyłaj tylko kod, do którego masz prawa. Kod skopiowany z innego projektu
zostaje na licencji tamtego projektu: musi być zgodna z MIT, a informacja
o autorach i licencji musi zostać zachowana. Taki kod opisz w opisie PR.
