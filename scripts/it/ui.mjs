// Проверка сайта бухгалтерии для связей SMS (docs/external-refs.md, разделы 3 и 5): значок «SMS» в ленте и в форме,
// правка операции в форме не стирает связь и заметку позиции. Нужен Playwright с Chromium.
//
//   source $WORK/env.sh && node scripts/it/ui.mjs
//
// Вход — «Вход для разработки» (APP_ENV=dev) под aleksey@demo.local. Скриншоты — в $IT_WORK.
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const API = (process.env.IT_API_URL || 'http://127.0.0.1:18080').replace(/\/$/, '');
const TOKEN = process.env.IT_TOKEN;
const WORK = process.env.IT_WORK || '/var/tmp/fms-it';
const today = new Date().toISOString().slice(0, 10);
const results = [];

async function api(method, path, body) {
  const r = await fetch(API + path, {
    method,
    headers: { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json' },
    body: body && JSON.stringify(body),
  });
  return [r.status, await r.json().catch(() => null)];
}

function check(name, ok, detail = '') {
  results.push(ok);
  console.log(`  ${ok ? '✅' : '❌'} ${name}${ok || !detail ? '' : `\n     ${detail}`}`);
}

const [st, tx] = await api('POST', '/api/transactions', {
  occurred_on: today, tx_kind: 'EXPENSE', account_code: 'CARD_KBANK', description: 'UI: из SMS', description_lang: 'RU',
  items: [{ category_code: 'FOOD_CAFE', amount: '401.41', note: 'SMS: 21/09/26 Withdrawal 401.41' }],
  external_ref: { provider: 'imessage', external_id: `ui-${Date.now()}`, received_at: new Date().toISOString() },
});
if (st !== 201) throw new Error(`создание операции: ${st} ${JSON.stringify(tx)}`);

const browser = await chromium.launch();
const page = await browser.newPage({ locale: 'ru-RU' });
await page.goto(`${API}/auth/login`);
await page.locator('form[action="/auth/dev"]').filter({ hasText: 'aleksey@demo.local' }).locator('button').click();
await page.waitForLoadState('networkidle');
// Токен выпущен в проекте «Семья»: переключиться на него
const family = page.locator('a[href^="/projects/switch/"]', { hasText: 'Семья' }).first();
if (await family.count()) await page.goto(API + (await family.getAttribute('href')));

console.log('\nСайт (docs/external-refs.md, 3 и 5):');
await page.goto(`${API}/transactions?account=CARD_KBANK`);
// Строка ленты: от ссылки на операцию вверх, пока не найдётся значок (или 8 уровней)
const title = await page.evaluate((id) => {
  let el = document.querySelector(`a[href*="/transactions/${id}"]`);
  for (let i = 0; el && i < 8; i++, el = el.parentElement) {
    const b = [...el.querySelectorAll('.badge')].find((x) => x.textContent.includes('SMS'));
    if (b) return b.getAttribute('title') || '';
  }
  return null;
}, tx.id);
check('С1 значок «SMS» в ленте у операции из SMS', title !== null);
check('С2 подсказка значка — «Из SMS банка · …»', /Из SMS банка/.test(title || ''), `title=${title}`);
await page.screenshot({ path: `${WORK}/ui-feed.png`, fullPage: true });

await page.goto(`${API}/transactions/${tx.id}/edit`);
check('С3 строка «SMS» в форме правки', (await page.locator('#tx').locator('xpath=preceding::span[contains(@class,"badge")][1]').filter({ hasText: 'SMS' }).count()) > 0
  || (await page.getByText(/Из SMS банка/).count()) > 0);
await page.screenshot({ path: `${WORK}/ui-form.png`, fullPage: true });

await page.locator('#tx [name="description"]').fill('UI: кофе, правка формой');
await Promise.all([page.waitForNavigation(), page.locator('#tx button.btn-primary').click()]);
const [, after] = await api('GET', `/api/transactions/${tx.id}`);
check('С4 правка формой сохранилась', after.description === 'UI: кофе, правка формой', `description=${after.description}`);
check('С5 после правки формой связь на месте', after.external_refs?.length === 1, JSON.stringify(after.external_refs));
check('С6 после правки формой заметка позиции на месте (Б2)', after.items?.[0]?.note === 'SMS: 21/09/26 Withdrawal 401.41', `note=${after.items?.[0]?.note}`);

await browser.close();
console.log(`\nИтого: ${results.filter(Boolean).length} из ${results.length} пройдено. Скриншоты: ${WORK}/ui-feed.png, ${WORK}/ui-form.png`);
process.exit(results.every(Boolean) ? 0 : 1);
