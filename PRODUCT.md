# Product

<!-- impeccable:product-schema 1 -->

## Platform

desktop (native, egui): Windows 10, Windows 11, macOS 14+

## Users

Mała grupa znajomych (do ok. 6 osób), która gra razem, rozmawia na głosowym
i ogląda nawzajem swoje streamy ekranu. Aplikacja jest otwarta godzinami
w tle obok gry lub przeglądarki. (Potwierdzone przez użytkownika.)

## Product Purpose

Czat tekstowy, głos i udostępnianie ekranu w jakości źródła, bez żadnego
własnego serwera. Sukces: stream ostry jak oryginał, zero konfiguracji,
dołączenie do pokoju jednym wklejonym zaproszeniem.

## Positioning

Każdy łączy się z każdym bezpośrednio (P2P, szyfrowane QUIC). Obraz jest
kodowany sprzętowo raz i trafia do widzów bajt w bajt, bez transkodowania.

## Operating Context

Ciemne pomieszczenie, monitor obok gry, okno często w połowie ekranu.
Jeden pokój na raz. Zaproszenie przesyła się poza aplikacją (np. komunikatorem).

## Capabilities and Constraints

- Pokój: nazwa, lista uczestników, kanał tekstowy `#czat`, jeden kanał głosowy.
- Głos: wyciszenie, wyłączenie dźwięku, wskaźnik mówienia, wybór urządzeń,
  aktywacja głosem albo „Naciśnij, aby mówić” z globalnym klawiszem.
- Stream: wybór ekranu, rozdzielczość, FPS, bitrate, kodek; oglądanie jednego streamu na raz.
- Bez historii czatu po wyjściu (brak serwera).
- UI po polsku.

## Brand Commitments

- Nazwa: BibiCord.
- Wzorzec jakości i układu: Discord (użytkownik: „chociaż w 80% tak dobre jak UI Discorda”).
- UI minimalistyczny, ciemny motyw.
