# Eve gelince Goblin'de bakılacaklar

Tarih: 2026-05-14 sabah
Build: ✅ temiz (59.47 KB CSS, 332.79 KB JS), `npm run build`
Tests: ✅ vitest 62/62, playwright 8/8 smoke
TS: ✅ no errors

**Mobilde göremedin, eve gelince `npm run tauri dev` ile aç ve sırayla bunlara bak. Hata gördüğün yere not düş, sabah devam ederiz.**

---

## 1. Canlı goblin karakteri (orta panel, GoblinLive)

Yeni karikatür SVG çizdim. Senin "karikatür gelmedi şu anki kötüydü" geri bildirimin sonrası → sen yeni karikatür/asset vereceksin. Şu anki SVG yer tutucudur.

**Kontrol et:**
- [ ] Yüz hatları (kulak, saç tutamı, gözler, burun, dudak, yanak rengi, çene gölgesi) doğru render mı?
- [ ] Konuştuğunda (streaming sırasında) **ağız** açılıp kapanıyor mu? (lipsync)
- [ ] Konuştuğunda **eller** sallanıyor mu? (gesture sway)
- [ ] State'e göre el pozisyonları değişiyor mu? (thinking → eli çenede, searching → eli gözüne siper, success → eller yukarı, error → kollar açık)
- [ ] Etrafta nabız atan yeşil halka var mı konuşurken?
- [ ] Arka plan hafif yeşil tona kayıyor mu konuşurken?
- [ ] "Speaking" etiketi aksent (yeşil) parlıyor mu?

**Sen karikatür asseti verince:** `src/assets/` altına atarız, `GoblinLive.tsx`'i ona bağlarım (PNG sprite, SVG, Lottie JSON, fark etmez).

**3D versiyonu (Goblin3D.tsx) yazdım ama Atakan "3D yapma" dedi → kullanılmıyor, build'e girmiyor. Silmedim, ileride lazım olabilir.**

---

## 2. Chat panel (sol)

### a. Header model dropdown (yeni)
- [ ] Sol üstte yeşil/mor pill → tıklayınca DeepSeek/Anthropic/NVIDIA/GLM gruplu menü açılıyor mu?
- [ ] Aktif model seçili (yeşil arka plan) görünüyor mu?
- [ ] Bir Anthropic modeli seçince pill rengi mora dönüyor mu? (DeepSeek=yeşil)
- [ ] Escape tuşu menüyü kapatıyor mu?

### b. TabBar (chat üstü)
- [ ] Aktif sekme **emerald yeşil** mi (eskiden maviydi)?
- [ ] Streaming sırasında aktif sekmede sarı nabız noktası beliriyor mu?
- [ ] Sekmede mesaj sayısı pill'i (sağda küçük rakam) doğru mu?
- [ ] `⌘1`, `⌘2`, `⌘3` … o sekmeye atlatıyor mu?

### c. Chat alanı
- [ ] Boş chat'te 4 starter chip görünüyor mu? Tıklanınca input'a metni yazıyor mu?
- [ ] Code block hover'da sağ üstte `📋` butonu çıkıyor mu? Tıklayınca clipboard'a yazıyor mu? `📋` → `✓` dönüyor mu?
- [ ] Code block sol üstte dil etiketi (örn. `BASH`, `TYPESCRIPT`) görünüyor mu?
- [ ] Streaming sırasında 3 noktalı "thinking bubble" beliriyor mu? Yanında active tool adı?
- [ ] Cevap bitince son asistan mesajının altında `↻ Continue` chip görünüyor mu?

### d. Input bar
- [ ] Dosyayı input alanına **sürükle-bırak** çalışıyor mu? Drop sırasında yeşil çerçeve + "📎 Drop file to attach" yazısı?
- [ ] Bırakınca input metnine `📎 dosyaadi (mime, KB)` ekleniyor mu?
- [ ] **Attach butonu** ile dosya seçince de aynı oluyor mu?
- [ ] Boş input'ta `/` tuşu komut paletini açıyor mu?
- [ ] `Enter` gönderiyor, `Shift+Enter` newline mı?
- [ ] Hint metni (`Enter send · ⇧Enter newline · ⌘K commands`) focus'ta görünür, blur'da gizleniyor mu?

---

## 3. Sağ panel (RightTabs)

- [ ] Dashboard sekmesinde 9 kart (Agent State / Model / Tokens In / Out / Total / Cost / Turns / Active Tool / Token Efficiency)?
- [ ] Mesajdaki tool badge'ine tıklayınca Output sekmesine atlıyor + tool args/result JSON dökülüyor mu?

---

## 4. Sidebar (sol drawer)

- [ ] `⌘⇧S` ile açılıyor mu?
- [ ] **Search input** geliyor mu? Yazınca sessions filter oluyor mu (title + model)?
- [ ] `×` butonu aramayı temizliyor mu?
- [ ] Match olmadığında "No matches for ..." mesajı?
- [ ] Provider listesi (DeepSeek/Anthropic/NVIDIA/Gemini/GLM) yeşil/kırmızı noktalı görünüyor mu?

---

## 5. WhatsApp paneli (header `💬` butonu)

- [ ] Slide-in animasyonu (sağdan kayarak) çalışıyor mu?
- [ ] Connected halde **contact arama** kutusu görünüyor mu?
- [ ] Her contact'ta **renkli avatar dairesi** (initials) var mı?
- [ ] Unread badge (yeşil daire) doğru sayı gösteriyor mu?
- [ ] Contact'a tıklayınca conv açılıyor → header'da avatar + ad + alt-bilgi?
- [ ] **Auto-reply açıkken** avatar'da online dot (yeşil küçük) var mı?
- [ ] Senin gönderdiğin mesajlarda altında **✓✓ status icon** (yeşil) var mı?
- [ ] Yeni konuşmada "No messages yet, say hi 👋" davet baloncuğu?
- [ ] Send butonu yuvarlak yeşil paper-plane SVG, hover'da yukarı pop?

---

## 6. Klavye kısayolları

- [ ] `⌘/` → Cheat sheet overlay açılıyor mu? 9 satır kısayol listesi?
- [ ] `⌘K` → Command palette
- [ ] `⌘N` → Yeni session
- [ ] `⌘⇧S` → Sidebar
- [ ] `⌘1-9` → Tab geçişi (sekmeler arası)
- [ ] `Esc` → Açık overlay'leri kapatıyor mu (palette, sidebar, shortcuts, model menu)?

---

## 7. Onboarding & Cost cap

- [ ] **İlk açılışta** (localStorage temizken) sağ altta 3 adımlı onboarding toast görünüyor mu? "Got it" tıklayınca kayboluyor mu? Bir daha açmıyor mu?
  - Reset için: DevTools yok dedin → `~/.goblin/.goblin/` ya da localStorage temizlemek için Tauri'yi tamamen kapatıp app verisini sil veya kodda `localStorage.clear()` çalıştır
- [ ] Maliyet `$0.50` aştığında sağ üstte sarı uyarı toast'u beliriyor mu? "Adjust cap" butonu Config'i açıyor mu?
- [ ] Panel genişliklerini değiştirip kapatıp açtığında **boyutlar korunuyor** mu? (localStorage)

---

## 8. Hata recovery

- [ ] Bir mesaj hata verirse alt status bar'da error pill yanında **`⟳ retry`** butonu var mı? Tıklayınca son user mesajını yeniden gönderiyor mu?

---

## 9. Attach bug (sınırlı çözüm)

**Önemli:** Şu an dosya yüklenince agent'a sadece **dosya adı + tip + boyut** notu gidiyor. Resmi gerçekten **görüntü olarak** göndermek için Rust backend'de:
- `send_message(attachments: Vec<{path, mime}>)` parametre eklemek
- Provider katmanına multimodal payload (OpenAI `image_url` / Anthropic image content block) iliştirmek

gerek. Bu ayrı bir sprint, Rust dokunmak gerekiyor. Şu an çalışan: Goblin "dosya yüklendi" haberini biliyor ama içeriğini göremiyor.

---

## 10. Henüz yapılmamışlar (memory transparency, onay diyalogu vs)

Quick-win listesinden yapılmayanlar (her biri orta/büyük iş):

- **#2 Aksiyon onay diyalogu**, bash/write_file gibi tool'lar için "Devam et / İptal" overlay (Cursor tarzı). **Backend hook gerek.**
- **#5 Memory transparency**, RightTabs'a "Context" sekmesi: bu turn'de hangi memory'ler inject edildi. **Backend log emit etmesi gerek.**
- **#12 WhatsApp log paneli**, bridge logları frontend'e stream. **Backend command gerek.**

Bunları yapmak istersen Rust'a dokunmak gerek, söyle başlayayım.

---

## Sabah sırası

Eve geldiğinde:
1. `cd ~/Projects/goblin && npm run tauri dev`
2. Yukarıdaki checklist'i sırayla geç
3. Pürüzlü gördüğün her noktaya `[ ]` yerine kısa not düş
4. Bana göster, sabah onlarla başlarız

Kod state'i: `git status` temiz, commit edilmedi (henüz). İstersen şimdi commit atayım, söyle.
