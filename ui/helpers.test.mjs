// The page uses local calendar days. Pin the zone before any Date call.
process.env.TZ = 'America/New_York';

import { randomUUID } from 'node:crypto';
import { readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

const html = readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'index.html'), 'utf8');

// Pull each top-level function out of the page so the test runs the real source.
// ponytail: the brace counter does not understand braces inside strings, regexes or comments; fine for these helpers; upgrade path is moving the helpers into a served ui/helpers.mjs module the page and this test both import.
function extractFunction(source, name) {
  const start = source.search(new RegExp(`function ${name}\\s*\\(`));
  if (start < 0) throw new Error(`ui/index.html has no function ${name}`);
  const open = source.indexOf('{', start);
  if (open < 0) throw new Error(`function ${name} has no body`);
  let depth = 0;
  for (let i = open; i < source.length; i++) {
    if (source[i] === '{') depth++;
    else if (source[i] === '}') {
      depth--;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  throw new Error(`function ${name} body did not close`);
}

// Smallest sibling of extractFunction for a top-level const object or array.
function extractConst(source, name) {
  const start = source.search(new RegExp(`const ${name}\\s*=`));
  if (start < 0) throw new Error(`ui/index.html has no const ${name}`);
  let depth = 0;
  let seen = false;
  for (let i = start; i < source.length; i++) {
    const c = source[i];
    if (c === '{' || c === '[' || c === '(') { depth++; seen = true; }
    else if (c === '}' || c === ']' || c === ')') depth--;
    else if (c === ';' && depth === 0 && seen) return source.slice(start, i + 1);
  }
  throw new Error(`const ${name} did not close`);
}

async function importTemp(source) {
  const file = join(tmpdir(), `tallimo-ui-${randomUUID()}.mjs`);
  writeFileSync(file, source);
  try {
    return await import(pathToFileURL(file));
  } finally {
    unlinkSync(file);
  }
}

async function loadHelpers(source) {
  const names = ['isoWeek', 'luFixCount', 'isoDay', 'mergedDay', 'mondayWindows', 'rangeOf', 'noteWindows', 'plural', 'luSplit', 'luHeading', 'luDay', 'luTimeline', 'luItemState', 'luCapacityLine', 'entryText', 'luLines', 'luNum', 'luItemFromFields', 'briefHeading', 'briefPartner', 'briefSay', 'briefRows', 'briefFyiLine', 'briefText', 'briefField', 'briefReadTag', 'briefItemHtml', 'briefSectionHtml', 'briefCard'];
  const body = [extractConst(source, 'LU_LABELS'), extractConst(source, 'A1_LABELS'), extractConst(source, 'esc')].concat(names.map((name) => extractFunction(source, name))).join('\n');
  const store = new Map();
  const localStorage = {
    getItem(key) {
      return store.has(key) ? store.get(key) : null;
    },
    setItem(key, value) {
      store.set(key, String(value));
    },
  };
  // The helpers close over this binding. The object is the test's store, so setItem and clear reach them.
  globalThis.localStorage = localStorage;
  const mod = await importTemp(`let localStorage = globalThis.localStorage;\n${body}\nexport { ${names.join(', ')}, A1_LABELS };\n`);
  for (const name of names) {
    if (typeof mod[name] !== 'function') throw new Error(`${name} did not evaluate`);
  }
  return { store, localStorage, ...mod };
}

const { store, localStorage, isoWeek, luFixCount, mergedDay, mondayWindows, noteWindows, plural, luHeading, luDay, luTimeline, luItemState, entryText, luLines, luNum, luItemFromFields, briefHeading, briefPartner, briefSay, briefRows, briefFyiLine, briefText, briefSectionHtml, briefCard, A1_LABELS } = await loadHelpers(html);

const THIS_WEEK = [
  { label: 'This week', from: '2026-10-05', to: '2026-10-11' },
  { label: 'Last week', from: '2026-09-28', to: '2026-10-04' },
];
const NEXT_WEEK = [
  { label: 'This week', from: '2026-10-12', to: '2026-10-18' },
  { label: 'Last week', from: '2026-10-05', to: '2026-10-11' },
];

// new Date() with no args is "now"; any other call still parses or copies.
function withNow(year, month, day, hour, fn) {
  const RealDate = Date;
  const fixed = new RealDate(year, month - 1, day, hour, 0, 0, 0);
  globalThis.Date = class extends RealDate {
    constructor(...args) {
      if (args.length === 0) super(fixed.getTime());
      else super(...args);
    }
    static now() {
      return fixed.getTime();
    }
  };
  try {
    return fn();
  } finally {
    globalThis.Date = RealDate;
  }
}

describe('ui date helpers', { concurrency: false }, () => {
  test('TZ=America/New_York took effect', () => {
    assert.equal(process.env.TZ, 'America/New_York');
    assert.equal(Intl.DateTimeFormat().resolvedOptions().timeZone, 'America/New_York');
    // 02:30Z is 22:30 the previous evening in EDT (UTC-4).
    const probe = new Date('2026-10-05T02:30:00Z');
    assert.equal(probe.getTimezoneOffset(), 240);
    assert.equal(probe.getDate(), 4);
    assert.equal(probe.getHours(), 22);
  });

  test('mergedDay reads a UTC instant as a local day', () => {
    assert.equal(mergedDay({ merged_at: '2026-10-05T02:30:00Z' }), '2026-10-04');
    assert.equal(mergedDay({ merged_at: '2026-10-05T04:00:00Z' }), '2026-10-05');
  });

  test('mondayWindows from Wednesday 2026-10-07 noon', () => {
    withNow(2026, 10, 7, 12, () => {
      assert.deepEqual(mondayWindows(), THIS_WEEK);
    });
  });

  test('mondayWindows from Sunday 2026-10-11 is still this week', () => {
    withNow(2026, 10, 11, 12, () => {
      assert.deepEqual(mondayWindows(), THIS_WEEK);
    });
  });

  test('mondayWindows from Monday 2026-10-12 starts the next week', () => {
    withNow(2026, 10, 12, 12, () => {
      assert.deepEqual(mondayWindows(), NEXT_WEEK);
    });
  });

  test('noteWindows returns the stored rollup.range', () => {
    localStorage.setItem('rollup.range', JSON.stringify({ from: '2026-03-02', to: '2026-03-08' }));
    assert.deepEqual(noteWindows(), [{ label: 'Range', from: '2026-03-02', to: '2026-03-08' }]);
  });

  test('noteWindows uses Monday windows when no range is stored', () => {
    store.clear();
    withNow(2026, 10, 7, 12, () => {
      assert.deepEqual(noteWindows(), THIS_WEEK);
    });
  });

  test('noteWindows falls back to Monday windows when rollup.range is malformed', () => {
    localStorage.setItem('rollup.range', '{');
    withNow(2026, 10, 7, 12, () => {
      assert.deepEqual(noteWindows(), THIS_WEEK);
    });
  });

  test('plural picks the singular only for 1', () => {
    assert.equal(plural(1, 'story', 'stories'), 'story');
    assert.equal(plural(2, 'story', 'stories'), 'stories');
  });
});

describe('ui flag helpers (human loop)', () => {
  async function loadFlag() {
    const src = `const FLAG_PAGES = ['today', 'inbox'];\n${extractFunction(html, 'flagCardId')}\n${extractFunction(html, 'flagBody')}\nexport { flagCardId, flagBody };\n`;
    return importTemp(src);
  }

  test('flagCardId keeps only the id characters the server accepts, 1 to 64 long', async () => {
    const { flagCardId } = await loadFlag();
    assert.equal(flagCardId('PAY-101'), 'PAY-101');
    assert.equal(flagCardId('#7 Fix the  ledger/../x'), '7-Fix-the-ledger..x');
    assert.equal(flagCardId(''), 'card');
    assert.equal(flagCardId('<>'), 'card');
    assert.equal(flagCardId('a'.repeat(100)).length, 64);
    assert.match(flagCardId('Nora Example: 3 open?'), /^[A-Za-z0-9._:-]{1,64}$/);
  });

  test('flagBody covers Today and Inbox only, always as a wrong flag', async () => {
    const { flagBody } = await loadFlag();
    assert.deepEqual(flagBody('today', 'PAY-101'), { page: 'today', card: 'PAY-101', verdict: 'wrong' });
    assert.deepEqual(flagBody('inbox', 'x y'), { page: 'inbox', card: 'x-y', verdict: 'wrong' });
    assert.equal(flagBody('leadership', 'x'), null);
    assert.equal(flagBody('room', 'x'), null);
  });

  test('Today and Inbox add flags; no other page does, and the flag posts to /feedback only', () => {
    for (const page of ['renderToday', 'renderInbox']) {
      assert.match(extractFunction(html, page), /addFlags\('(today|inbox)'\)/, page);
    }
    for (const page of ['renderRoom', 'renderDesk', 'renderUpdate', 'renderDigest', 'renderPerson', 'renderProject']) {
      assert.doesNotMatch(extractFunction(html, page), /addFlags/, page);
    }
    const send = extractFunction(html, 'sendFlag');
    assert.match(send, /postJson\('\/feedback'/);
    assert.doesNotMatch(send, /https?:/);
  });
});

const SAMPLE_ENTRY_TEXT = `Mar 16, 2029
Team capacity 83% · 17% free

GROWTH PROGRAMME
Demo Vendor (MVP) ★ Project K NEW
TIMELINE: Jan 8 → Mar 16
% COMPLETE: 100
CAPACITY: 25%
COMPLETED THIS SPRINT:
1. The MVP launched successfully on Mar 16.
2. The launch moved from Mar 14 to Mar 16 to absorb late requests from the client team.
3. Weekly onboarding sessions are already running.
PLANNED NEXT SPRINT:
1. Run the retrospective next week.
2. Finish the user guide.

Sample Advisor ★ Project K NEW
% COMPLETE: 40
CAPACITY: 38%
COMPLETED THIS SPRINT:
1. Three of the eight team members are now working on Sample Advisor.
2. Access to both the production and UAT environments is in hand.
3. The UAT completion date has moved from Apr 1 to Apr 15, because a weekly cadence with the partner team has not been possible.
PLANNED NEXT SPRINT:
1. Hold Apr 15 as the internal target for UAT completion.
2. Ask for leadership: support in securing a weekly slot with the partner team.

SCREENING
Example Casework, Phase 1 MVP NEW
TIMELINE: Oct 1, 2028 → May 15, 2029
% COMPLETE: 90
CAPACITY: 15%
COMPLETED THIS SPRINT:
1. Two pieces of the original plan remain, the message queue and the metrics dashboard, and both are in active build.
PLANNED NEXT SPRINT:
1. Complete both, targeting Apr 30 and no later than May 15.

LEADERSHIP TOPICS
Example Corp Contract Renegotiation NEW
% COMPLETE: 85
CAPACITY: 5%
COMPLETED THIS SPRINT:
1. Pricing is agreed. The existing master agreement stays in place.
PLANNED NEXT SPRINT:
1. Meet Taylor next week to finalise the contract, expected to take about a month.

Additional December Leave for the Team NEW
COMPLETED THIS SPRINT:
1. For information: additional leave is planned for the team members who committed exceptionally this year.
2. The leave extends the standard December break, so capacity drops earlier than usual.
3. The aim is to keep morale going, and the owner hopes the Director will support it.`;

describe('luItemState', () => {
  test('pct 100 is Done even when the end date is past', () => {
    assert.deepEqual(luItemState({pct: 100, end_date: '2029-02-27'}, '2029-03-02'), {mark: '●', word: 'Done', cls: 'is-done'});
  });

  test('late counts whole UTC days across a month boundary', () => {
    assert.deepEqual(luItemState({pct: 10, end_date: '2029-02-27'}, '2029-03-02'), {mark: '▲', word: 'Late 3d', cls: 'is-late'});
    assert.deepEqual(luItemState({pct: 10, end_date: '2029-02-27'}, '2029-03-02T04:00:00Z'), {mark: '▲', word: 'Late 3d', cls: 'is-late'});
  });

  test('a tracked item with no end date needs one, and keeps the percent', () => {
    assert.deepEqual(luItemState({pct: 40, end_date: null}, '2029-03-02'), {mark: '◇', word: 'Needs an end date · 40%', cls: 'is-needs'});
    assert.deepEqual(luItemState({pct: 10, end_date: null}, '2029-03-02'), {mark: '◇', word: 'Needs an end date · 10%', cls: 'is-needs'});
    assert.deepEqual(luItemState({pct: null, start_date: '2029-01-08', end_date: ''}, '2029-03-02'), {mark: '◇', word: 'Needs an end date', cls: 'is-needs'});
  });

  test('an item with no dates and no percent has no state', () => {
    assert.equal(luItemState({pct: null, end_date: ''}, '2029-03-02'), null);
    assert.equal(luItemState({pct: 0, start_date: null, end_date: null}, '2029-03-02'), null);
  });

  test('in progress uses the percent only when it is set', () => {
    assert.deepEqual(luItemState({pct: 40, start_date: '2029-06-01', end_date: '2029-08-01'}, '2029-03-02'), {mark: '◐', word: 'In progress · 40%', cls: 'is-progress'});
    assert.deepEqual(luItemState({pct: null, start_date: '2029-01-08', end_date: '2029-08-01'}, '2029-03-02'), {mark: '◐', word: 'In progress', cls: 'is-progress'});
  });

  test('otherwise the item has not started', () => {
    assert.deepEqual(luItemState({pct: null, start_date: null, end_date: '2029-08-01'}, '2029-03-02'), {mark: '○', word: 'Not started', cls: 'is-idle'});
  });
});

describe('leadership update summary', () => {
  test('the sentence counts what is on track and names what is not', async () => {
    const body = `${extractFunction(html, 'luItemState')}\n${extractFunction(html, 'luLateDays')}\n${extractFunction(html, 'luSummaryLine')}`;
    const { luSummaryLine } = await importTemp(`${body}\nexport { luSummaryLine };`);
    const items = [
      { title: 'Payout reports', pct: 40, start_date: '2029-02-01', end_date: '2029-02-27' },
      { title: 'Nightly jobs', pct: 20, start_date: '2029-02-01', end_date: null },
      { title: 'Access review', pct: 100, end_date: '2029-02-20' },
      { title: 'Card refunds', pct: 60, start_date: '2029-02-01', end_date: '2029-03-30' },
    ];
    assert.equal(luSummaryLine(items, '2029-03-02'), '4 items this week. 2 are on track. Payout reports is 3 days late, and Nightly jobs needs an end date.');
    assert.equal(luSummaryLine([], '2029-03-02'), 'No items this week.');
  });

  test('the capacity split sums each project and leaves the free share', async () => {
    const body = `${extractFunction(html, 'luCapacitySplit')}`;
    const { luCapacitySplit } = await importTemp(`${body}\nexport { luCapacitySplit };`);
    const split = luCapacitySplit([
      { category: 'Payments', capacity: 30 },
      { category: 'Payments', capacity: 20 },
      { category: 'Audit', capacity: 15 },
      { category: 'Platform', capacity: null },
    ]);
    assert.deepEqual(split, { rows: [{ name: 'Payments', pct: 50 }, { name: 'Audit', pct: 15 }], used: 65, free: 35 });
  });
});

describe('leadership update copy', () => {
  test('luHeading drops the ordinal', () => {
    const cases = [
      ['2029-03-01', 'Mar 1, 2029'],
      ['2029-03-02', 'Mar 2, 2029'],
      ['2029-03-03', 'Mar 3, 2029'],
      ['2029-03-11', 'Mar 11, 2029'],
      ['2029-03-12', 'Mar 12, 2029'],
      ['2029-03-13', 'Mar 13, 2029'],
      ['2029-03-22', 'Mar 22, 2029'],
      ['2029-03-31', 'Mar 31, 2029'],
    ];
    for (const [iso, heading] of cases) assert.equal(luHeading(iso), heading, iso);
  });

  test('luDay formats a calendar day and treats null as empty', () => {
    assert.equal(luDay('2029-01-08'), 'Jan 8, 2029');
    assert.equal(luDay(null), '');
  });

  test('luTimeline shows a shared year only when it is not the entry year', () => {
    assert.equal(luTimeline({ start_date: '2029-01-08', end_date: '2029-03-16' }, '2029-03-16'), 'Jan 8 → Mar 16');
    assert.equal(luTimeline({ start_date: '2028-01-08', end_date: '2028-03-16' }, '2029-03-16'), 'Jan 8 → Mar 16, 2028');
    assert.equal(luTimeline({ start_date: '2028-12-08', end_date: '2029-03-16' }, '2029-03-16'), 'Dec 8, 2028 → Mar 16, 2029');
    assert.equal(luTimeline({ start_date: null, end_date: '2029-03-16' }, '2029-05-15'), '→ Mar 16');
    assert.equal(luTimeline({ start_date: '2029-01-08', end_date: '' }, '2029-05-15'), 'Jan 8 →');
  });

  test('luTimeline keeps a one-sided year when it is not the entry year', () => {
    assert.equal(luTimeline({ start_date: '2028-01-08', end_date: null }, '2029-05-15'), 'Jan 8, 2028 →');
    assert.equal(luTimeline({ start_date: null, end_date: '2029-05-15' }, '2029-05-15'), '→ May 15');
    assert.equal(luTimeline({ start_date: null, end_date: '2028-05-15' }, '2029-05-15'), '→ May 15, 2028');
  });

  test('entryText copies the 2029-03-16 sample', () => {
    const sample = JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../fixtures/sample.json'), 'utf8'));
    const raw = sample.leadership_updates.entries.find((entry) => entry.entry_date === '2029-03-16');
    const entry = {
      entry_date: raw.entry_date,
      programme: raw.programme,
      total: 83,
      label: '17% unallocated',
      items: raw.items.map((item) => ({ ...item, is_new: true })),
    };
    assert.equal(entryText(entry), SAMPLE_ENTRY_TEXT);
  });

  test('entryText drops empty fields and keeps a one-sided timeline', () => {
    const text = entryText({
      entry_date: '2029-05-15',
      total: null,
      label: null,
      programme: null,
      items: [
        {
          category: 'Screening',
          title: 'Only start',
          priority: 0,
          start_date: '2029-01-08',
          end_date: null,
          pct: null,
          capacity: null,
          completed: [],
          next: [],
          future: ['Later'],
          is_new: false,
        },
        {
          category: 'Screening',
          title: 'Only end',
          priority: 1,
          start_date: null,
          end_date: '2029-05-15',
          pct: 0,
          capacity: 0,
          completed: [],
          next: [],
          future: [],
          is_new: true,
        },
      ],
    });
    assert.equal(text, [
      'May 15, 2029',
      '',
      'SCREENING',
      'Only start',
      'TIMELINE: Jan 8 →',
      'PLANNED FOR FUTURE:',
      '1. Later',
      '',
      'Only end NEW',
      'TIMELINE: → May 15',
      '% COMPLETE: 0',
      'CAPACITY: 0%',
    ].join('\n'));
  });
});

describe('leadership update editor helpers', () => {
  test('luLines drops blank lines and trims the rest', () => {
    assert.deepEqual(luLines('  a \n\n b\n\n'), ['a', 'b']);
    assert.deepEqual(luLines(''), []);
    assert.deepEqual(luLines(' \n \n'), []);
  });

  test('luNum parses numbers and turns blanks into null', () => {
    assert.equal(luNum('25'), 25);
    assert.equal(luNum(' 0 '), 0);
    assert.equal(luNum(''), null);
    assert.equal(luNum('   '), null);
    assert.equal(luNum(null), null);
    assert.ok(Number.isNaN(luNum('nope')));
  });

  test('luItemFromFields builds the item body', () => {
    assert.deepEqual(luItemFromFields({
      item_id: 'pay-1',
      category: 'Now',
      position: '2',
      title: 'Payments',
      priority: true,
      start_date: '',
      end_date: '2029-03-16',
      pct: '',
      capacity: '25',
      completed: 'one\n\n two \n',
      next: '',
      future: 'later',
    }), {
      item_id: 'pay-1',
      category: 'Now',
      position: 2,
      title: 'Payments',
      priority: 1,
      start_date: null,
      end_date: '2029-03-16',
      pct: null,
      capacity: 25,
      completed: ['one', 'two'],
      next: [],
      future: ['later'],
    });
  });
});

function withAsync(source, name) {
  const fn = extractFunction(source, name);
  return new RegExp(`\\basync\\s+function\\s+${name}\\b`).test(source) ? `async ${fn}` : fn;
}

async function loadLuSubmit(source) {
  const body = `${extractFunction(source, 'plural')}\n${extractFunction(source, 'luFail')}\n${withAsync(source, 'luSubmit')}`;
  return importTemp(`let postJson, note, route, renderUpdates, luOwnerId, luFieldsOf, luFreshId, luConfirmDirty, window;\n${body}\nexport function bind(deps){ ({postJson, note, route, renderUpdates, luOwnerId, luFieldsOf, luFreshId, luConfirmDirty = () => true, window} = deps); }\nexport { luSubmit };\n`);
}

const { bind, luSubmit } = await loadLuSubmit(html);

function correctForm(fields, checked) {
  const btn = {disabled: !checked};
  return {
    btn,
    getAttribute(name) { return name === 'data-lu' ? 'correct' : null; },
    elements: {namedItem(name) { return Object.hasOwn(fields, name) ? {value: fields[name]} : null; }},
    querySelector(sel) {
      if (sel === 'button[type="submit"]') return btn;
      if (sel === '[data-lu-ack]') return {checked};
      return null;
    },
  };
}

function track(replies, ok) {
  const posts = [], notes = [], asks = [], state = {rendered: 0};
  bind({
    async postJson(url, body) {
      posts.push({url, body});
      const next = replies.shift();
      if (next instanceof Error) throw next;
      return next;
    },
    note(msg, bad) { notes.push([msg, !!bad]); },
    route: () => ({name: 'updates'}),
    renderUpdates() { state.rendered += 1; },
    luOwnerId: () => 'pm-1',
    luFieldsOf() { throw new Error('item'); },
    luFreshId() { throw new Error('item'); },
    window: {confirm(msg) { asks.push(msg); return ok; }},
  });
  return {posts, notes, asks, state};
}

describe('leadership update correction preview', () => {
  test('preview gates the write', async () => {
    const idle = track([], false);
    const locked = correctForm({find: 'vendor', replace: 'partner'}, false);
    await luSubmit(locked);
    assert.equal(idle.posts.length, 0);
    assert.equal(locked.btn.disabled, true);

    const none = track([{would_change: 0}], false);
    const blank = correctForm({find: 'vendor'}, true);
    await luSubmit(blank);
    assert.deepEqual(none.posts, [{url: '/updates/correct', body: {owner: 'pm-1', find: 'vendor', replace: '', preview: true}}]);
    assert.deepEqual(none.notes, [['No entry contains that text.', true]]);
    assert.equal(none.asks.length, 0);
    assert.equal(none.state.rendered, 0);
    assert.equal(blank.btn.disabled, false);

    const cancelled = track([{would_change: 3}], false);
    const kept = correctForm({find: 'vendor', replace: 'partner'}, true);
    await luSubmit(kept);
    assert.equal(cancelled.posts.length, 1);
    assert.equal(cancelled.asks[0], 'This changes 3 rows across sent entries. Apply it?');
    assert.equal(cancelled.notes.length, 0);
    assert.equal(cancelled.state.rendered, 0);
    assert.equal(kept.btn.disabled, false);

    const applied = track([{would_change: 1}, {changed: 1}], true);
    await luSubmit(correctForm({find: 'vendor', replace: ''}, true));
    assert.deepEqual(applied.posts[0].body, {owner: 'pm-1', find: 'vendor', replace: '', preview: true});
    assert.deepEqual(applied.posts[1].body, {owner: 'pm-1', find: 'vendor', replace: '', confirm: true});
    assert.equal(applied.asks[0], 'This changes 1 row across sent entries. Apply it?');
    assert.deepEqual(applied.notes, [['Changed 1 row.', false]]);
    assert.equal(applied.state.rendered, 1);
  });

  test('a network error during preview leaves the button enabled, renders nothing and calls luFail once', async () => {
    const failed = track([new Error('network')], false);
    const form = correctForm({find: 'vendor', replace: 'partner'}, true);
    await luSubmit(form);
    assert.equal(form.btn.disabled, false);
    assert.equal(failed.state.rendered, 0);
    assert.equal(failed.notes.length, 1);
    assert.deepEqual(failed.notes, [['network', true]]);
  });
});

describe('leadership update load states', () => {
  test('the page shows a loading line, then an error with retry', () => {
    const start = html.indexOf('function renderUpdates(');
    const body = html.slice(start, html.indexOf('\nfunction ', start + 1));
    const loading = '<p class="lu-loading">Loading leadership updates.</p>';
    assert.ok(body.indexOf(loading) >= 0 && body.indexOf(loading) < body.indexOf("jf('/updates?who="));
    assert.ok(body.includes('class="lu-error"') && body.includes('Could not load leadership updates. ') && body.includes('data-lu-retry>Try again'));
    const click = html.slice(html.indexOf("addEventListener('click'"), html.indexOf("addEventListener('submit'"));
    assert.match(click, /data-lu-retry[\s\S]*renderUpdates\(\)/);
    assert.match(html, /\.lu-loading\{color:var\(--ink-3\)\}/);
    assert.match(html, /\.lu-error\{[^}]*color:var\(--ink\)/);
  });
});

describe('mark sent while a save is in flight', () => {
  test('luSaveInFlight is true only while an item form is saving', async () => {
    const { luSaveInFlight } = await importTemp(`${extractFunction(html, 'luSaveInFlight')}\nexport { luSaveInFlight };\n`);
    const sel = 'form[data-lu="item"][data-saving]';
    assert.equal(luSaveInFlight({ querySelector(q) { assert.equal(q, sel); return null; } }), false);
    assert.equal(luSaveInFlight({ querySelector(q) { assert.equal(q, sel); return {}; } }), true);
    const send = extractFunction(html, 'luSend');
    assert.ok(send.includes('luSaveInFlight(document)'));
    assert.ok(send.includes('An item is still saving. Wait a moment, then mark sent.'));
  });

  test('a form with only data-unsent does not count as in flight', async () => {
    const { luSaveInFlight } = await importTemp(`${extractFunction(html, 'luSaveInFlight')}\nexport { luSaveInFlight };\n`);
    const failed = { 'data-lu': 'item', 'data-unsent': '1' };
    const root = {
      querySelector(q) {
        assert.equal(q, 'form[data-lu="item"][data-saving]');
        return failed['data-lu'] === 'item' && failed['data-saving'] ? failed : null;
      },
    };
    assert.equal(luSaveInFlight(root), false);
  });
});

const A1_LABEL_TEXT = [
  'Weekly readout',
  'Partner brief',
  'Exception-based updates for <partner>: intervention needed first, visibility items second.',
  'Week ending',
  'Prepared by',
  'Bottom line',
  '01',
  'Escalations, blockers, or issues',
  "Include only items that need <partner>'s intervention, air cover, decision, or early awareness.",
  '02',
  'Key updates and FYIs',
  'Share meaningful outcomes, decisions, milestones, or emerging topics, not routine activity.',
  'None this week',
  'Needs attention',
  'FYI',
  'Issue or blocker',
  'Impact if unresolved',
  'Support requested',
  'What changed',
  'Why <partner> should know',
  'Next milestone',
  'No action required from <partner>',
  ' at this stage',
  '.',
  'Lead with change. Start with the decision, risk, or outcome, not project history.',
  'Make the "so what" explicit. Connect every bullet to impact, exposure, or stakeholder relevance.',
  'Close with action. Name the request and deadline, or say "no action required."',
  '<org unit> · Internal working brief',
  'Prepared for Partner Office · Friday cadence',
];

const A1_EXAMPLE = `Example Division · Weekly readout
Partner brief
Exception-based updates for Alex: intervention needed first, visibility items second.
Week ending Friday, March 9, 2029
Prepared by Sam Sample

Bottom line
Two March launches remain on track, and the Example Corp renegotiation is expected to save approximately $50,000 a year. The principal risk is the loss of contractor capacity, which will push delivery dates until replacement staff are confirmed.

01  Escalations, blockers, or issues

    Needs attention
    Replacement staff needed to protect priority 2029 delivery
      Issue or blocker: The portfolio has already been reduced, and the team has now been asked to release its contractor staffing. Detailed replacement requests have been sent to Jordan Example for allocation.
      Impact if unresolved: The team cannot give reliable delivery dates until replacements are onboarded, and major delays are expected across the top-priority 2029 projects.
      Support requested: Reinforce the urgency of the staffing allocation and support resetting stakeholder timelines until replacements are confirmed.

02  Key updates and FYIs

    FYI
    Example Casework launches March 12
      What changed: The screening team is ready to launch Example Casework on March 12. Final demos are under way and the launch email goes out shortly.
      Why Alex should know: This is the screening team's largest delivery of the year and moves the new workflow into production.
      Next milestone: Complete final demos and send launch communications before go-live. No action required from Alex.

    FYI
    Demo Vendor version one launches March 22
      What changed: The first version of Demo Vendor is on track for March 22, with no current blockers.
      Why Alex should know: It is the first phase of a wider due-diligence expansion across suppliers, advisers and partners.
      Next milestone: Complete readiness activities and launch on March 22. No action required from Alex.

    FYI
    Example Corp renegotiation targets approximately $50,000 in annual savings
      What changed: The team is renegotiating a Example Corp contract worth about $100,000, with expected annual savings of approximately $50,000.
      Why Alex should know: The scope expands capability while reducing future reliance on contractors.
      Next milestone: Complete commercial negotiations and confirm final scope and savings. No action required from Alex at this stage.`;

function trimmedLines(text) {
  return String(text).split('\n').map((line) => line.trim()).filter((line) => line !== '');
}

function assertInOrder(haystack, needles) {
  let at = 0;
  for (const line of haystack) {
    if (at < needles.length && line === needles[at]) at += 1;
  }
  assert.equal(at, needles.length, needles[at] ? `missing line: ${needles[at]}` : 'order');
}

describe('partner brief copy', () => {
  test('A1_LABELS holds every template label and nothing else', () => {
    // A1_LABEL_TEXT is the template's wording, so compare it with the label values. A label added to only one side fails.
    assert.deepEqual(new Set(Object.values(A1_LABELS)), new Set(A1_LABEL_TEXT));
  });

  test('briefHeading uses the UTC weekday', () => {
    assert.equal(briefHeading('2029-03-09'), 'Friday, March 9, 2029');
    assert.equal(briefHeading('2029-03-12'), 'Monday, March 12, 2029');
  });

  test('briefText of the fixture follows the example, plus the lines the example omits', () => {
    const sample = JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../fixtures/sample.json'), 'utf8'));
    const brief = sample.partner_briefs.briefs.find((row) => row.week_ending === '2029-03-09');
    assert.ok(brief, 'fixtures/sample.json has no partner brief for 2029-03-09');
    const lines = trimmedLines(briefText(brief));
    assertInOrder(lines, trimmedLines(A1_EXAMPLE));
    for (const extra of [
      "Include only items that need Alex's intervention, air cover, decision, or early awareness.",
      'Share meaningful outcomes, decisions, milestones, or emerging topics, not routine activity.',
      'Lead with change. Start with the decision, risk, or outcome, not project history.',
      'Make the "so what" explicit. Connect every bullet to impact, exposure, or stakeholder relevance.',
      'Close with action. Name the request and deadline, or say "no action required."',
      'Example Division · Internal working brief',
      'Prepared for Partner Office · Friday cadence',
    ]) assert.ok(lines.includes(extra), extra);
  });

  test('an empty section says none this week and a numeric flag keeps the stage line', () => {
    const lines = trimmedLines(briefText({
      week_ending: '2029-03-10',
      org_unit: 'Example Division',
      partner_name: 'Alex',
      prepared_by: 'Sam Sample',
      bottom_line: 'Quiet week.',
      items: [{ kind: 'fyi', position: 0, headline: 'One update', what: 'Something shipped.', why: 'It matters.', action: 'Watch the launch.', at_this_stage: 1 }],
    }));
    assert.equal(briefHeading('2029-03-10'), 'Saturday, March 10, 2029');
    assert.ok(lines.includes('None this week'));
    assert.ok(lines.includes('Next milestone: Watch the launch. No action required from Alex at this stage.'));
    assert.equal(lines.filter((line) => line === 'None this week').length, 1);
  });

  test('renderBrief loads sent briefs, with the same loading and error states', () => {
    const start = html.indexOf('function renderBrief(');
    const body = html.slice(start, html.indexOf('\nfunction ', start + 1));
    assert.ok(body.includes("jf('/briefs?who="));
    assert.ok(body.includes('class="lu-loading"') && body.includes('Loading the partner brief.'));
    assert.ok(body.includes('class="lu-error"') && body.includes('Could not load the partner brief. ') && body.includes('data-brief-retry>Try again'));
    assert.ok(body.includes('class="lu-wait">Briefs appear here once the owner sends one.'));
    assert.ok(body.includes('brief.sent_at'));
    const click = html.slice(html.indexOf("addEventListener('click'"), html.indexOf("addEventListener('submit'"));
    assert.match(click, /data-brief-retry[\s\S]*renderBrief\(\)/);
    assert.match(click, /data-brief-copy[\s\S]*briefText\(brief\)/);
  });
});

const QUIET_BRIEF = {
  week_ending: '2029-03-10',
  org_unit: 'Example Division',
  partner_name: 'Alex',
  prepared_by: 'Sam Sample',
  bottom_line: 'Quiet week.',
  items: [],
};

describe('partner brief helpers', () => {
  test('briefPartner reads partner_name and tolerates a missing one', () => {
    assert.equal(briefPartner({ partner_name: 'Alex' }), 'Alex');
    assert.equal(briefPartner({ partner_name: null }), '');
    assert.equal(briefPartner(null), '');
  });

  test('briefSay fills every <partner> and <org unit>, and blanks a missing value', () => {
    assert.equal(briefSay('<partner> asked <partner> in <org unit>', 'Alex', 'Example Division'), 'Alex asked Alex in Example Division');
    assert.equal(briefSay('For <partner> in <org unit>.', null), 'For  in .');
  });

  test('briefRows keeps one kind, sorts by numeric position and skips junk items', () => {
    const brief = { items: [
      { kind: 'fyi', position: 10, headline: 'c' },
      null,
      { kind: 'needs', position: 0, headline: 'x' },
      { kind: 'fyi', position: 2, headline: 'b' },
    ] };
    assert.deepEqual(briefRows(brief, 'fyi').map((row) => row.headline), ['b', 'c']);
    assert.deepEqual(briefRows(brief, 'needs').map((row) => row.headline), ['x']);
    assert.deepEqual(briefRows(null, 'fyi'), []);
  });

  test('briefFyiLine joins the action, the close and the stage flag', () => {
    assert.equal(briefFyiLine({ action: 'Ship it.', at_this_stage: 0 }, 'Alex'), 'Next milestone: Ship it. No action required from Alex.');
    assert.equal(briefFyiLine({ action: 'Ship it.', at_this_stage: 1 }, 'Alex'), 'Next milestone: Ship it. No action required from Alex at this stage.');
  });

  test('briefFyiLine leaves no double space when the action is empty', () => {
    for (const item of [{ action: '' }, { action: '   ' }, { action: null }, {}, null]) {
      assert.equal(briefFyiLine(item, 'Alex'), 'Next milestone: No action required from Alex.', JSON.stringify(item));
    }
    assert.equal(briefFyiLine({ action: ' ', at_this_stage: true }, 'Alex'), 'Next milestone: No action required from Alex at this stage.');
  });

  test('the read card keeps section titles and drops the help lines', () => {
    const card = briefCard({ ...QUIET_BRIEF, partner_name: 'Alex' }, 0);
    const section = briefSectionHtml({ ...QUIET_BRIEF, items: [{ kind: 'needs', position: 0, headline: 'One call.' }] }, 'needs');
    assert.match(card, />Needs your decision <span>0<\/span></);
    assert.match(card, />Worth knowing <span>0 · no action needed<\/span></);
    assert.match(section, />Needs your decision <span>1<\/span></);
    assert.equal(card.includes('brief-help'), false);
    assert.equal(section.includes('brief-help'), false);
    assert.equal(card.includes(briefSay(A1_LABELS.s1help, 'Alex')), false);
    assert.equal(card.includes(A1_LABELS.s2help), false);
  });

  test('the copy text and the card both drop an empty action cleanly', () => {
    const brief = { ...QUIET_BRIEF, items: [{ kind: 'fyi', position: 0, headline: 'One update', what: 'Something shipped.', why: 'It matters.', action: '', at_this_stage: 0 }] };
    assert.ok(trimmedLines(briefText(brief)).includes('Next milestone: No action required from Alex.'));
    const card = briefCard(brief, 0);
    assert.equal(card.includes('memo-next'), false);
    assert.ok(card.includes('Something shipped. It matters.'));
  });

  test('a week_ending briefHeading cannot format shows as written, in the card and in the copy text', () => {
    assert.ok(briefCard({ ...QUIET_BRIEF, week_ending: '2029-03-09' }, 0).includes('<p class="memo-eye">Partner brief · week ending Friday, March 9, 2029</p>'));
    for (const week of ['2029-3-9', '2029-02-30', 'soon']) {
      const card = briefCard({ ...QUIET_BRIEF, week_ending: week }, 0);
      assert.equal(briefHeading(week), '', week);
      assert.ok(card.includes(`<p class="memo-eye">Partner brief · week ending ${week}</p>`), week);
      assert.ok(card.includes(`aria-label="Copy Week ending ${week}"`), week);
      assert.ok(trimmedLines(briefText({ ...QUIET_BRIEF, week_ending: week })).includes(`Week ending ${week}`), week);
    }
    // The raw text goes into the page, so it must still be escaped.
    assert.ok(briefCard({ ...QUIET_BRIEF, week_ending: '<b>&' }, 0).includes('<p class="memo-eye">Partner brief · week ending &lt;b&gt;&amp;</p>'));
  });
});

describe('access link', () => {
  test('takeAccessToken returns the token and ignores junk', async () => {
    const token = 'ab'.repeat(32);
    assert.equal(token.length, 64);
    assert.ok(html.includes('Open Tallimo from its private link'));
    assert.ok(html.includes('data-access-copy'));
    assert.ok(html.includes('./rollup-api open'));
    const fn = extractFunction(html, 'takeAccessToken');
    const mod = await importTemp(`${fn}\nexport { takeAccessToken };\n`);
    assert.equal(mod.takeAccessToken(`#/updates?t=${token}`), token);
    assert.equal(mod.takeAccessToken(`#t=${token}`), token);
    assert.equal(mod.takeAccessToken('#/updates?t=nope'), '');
    assert.equal(mod.takeAccessToken(`#t=${token}ff`), '');
  });

  test('captureAccessToken stores the token and strips the whole fragment from the URL', async () => {
    const token = 'ab'.repeat(32);
    const other = 'cd'.repeat(32);
    const fns = ['takeAccessToken', 'tallimoToken', 'captureAccessToken'].map((name) => extractFunction(html, name)).join('\n');
    const page = await importTemp(`let memToken = '';\n${fns}\nexport { captureAccessToken, tallimoToken };\n`);
    // The page reads location, history and sessionStorage as globals. Stub them here and put them back in finally.
    const names = ['location', 'history', 'sessionStorage'];
    const saved = names.map((name) => Object.getOwnPropertyDescriptor(globalThis, name));
    const stub = (name, value) => Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
    try {
      const open = (hash, storage) => {
        const calls = [];
        const stored = new Map();
        stub('location', { pathname: '/', search: '?who=mina', hash });
        stub('history', { replaceState: (...args) => calls.push(args) });
        stub('sessionStorage', storage || { getItem: (key) => stored.get(key) ?? null, setItem: (key, value) => stored.set(key, value) });
        page.captureAccessToken();
        return { calls, stored };
      };
      for (const hash of [`#t=${token}`, `#/updates?t=${token}`]) {
        const { calls, stored } = open(hash);
        assert.deepEqual(calls, [[null, '', '/?who=mina']], hash);
        assert.doesNotMatch(calls[0][2], /#|t=/, hash);
        assert.equal(stored.get('tallimo.token'), token, hash);
        assert.equal(page.tallimoToken(), token, hash);
      }
      // A hash with no token is left alone.
      assert.deepEqual(open('#/updates').calls, []);
      // If sessionStorage throws, the in-memory copy still holds the token and the strip still runs.
      // A different token, because the module keeps its copy between cases and the old one must not stand in.
      const blocked = { getItem() { throw new Error('blocked'); }, setItem() { throw new Error('blocked'); } };
      const kept = open(`#t=${other}`, blocked);
      assert.equal(page.tallimoToken(), other);
      assert.equal(kept.calls.length, 1);
      // The hashchange listener goes through captureAccessToken, so it strips the same way.
      assert.match(html, /addEventListener\('hashchange', \(\) => \{\s*if \(!takeAccessToken\(location\.hash\)\) return;\s*captureAccessToken\(\);\s*location\.reload\(\);/);
    } finally {
      names.forEach((name, i) => {
        if (saved[i]) Object.defineProperty(globalThis, name, saved[i]);
        else delete globalThis[name];
      });
    }
  });
});

describe('ai page helpers', () => {
  test('run counts read as accepted and rejected', async () => {
    const fn = extractFunction(html, 'runCounts');
    const mod = await importTemp(`${fn}\nexport { runCounts };\n`);
    assert.equal(mod.runCounts({ accepted: 5, rejected: 1 }), '5 accepted, 1 rejected');
    assert.equal(mod.runCounts({ accepted: 0, rejected: 0 }), '0 accepted, 0 rejected');
  });

  test('outcome words match the stored outcomes', async () => {
    const fn = extractFunction(html, 'outcomeWord');
    const mod = await importTemp(`${fn}\nexport { outcomeWord };\n`);
    assert.equal(mod.outcomeWord('no_draft'), 'No draft');
    assert.equal(mod.outcomeWord('drafted'), 'Drafted');
    assert.equal(mod.outcomeWord('unchanged'), 'Unchanged');
    assert.equal(mod.outcomeWord('failed'), 'Failed');
  });

  test('the off status line names the setting', async () => {
    const fn = extractFunction(html, 'statusLine');
    const mod = await importTemp(`${fn}\nexport { statusLine };\n`);
    const line = mod.statusLine({ scheduled: 'off', publish_mode: 'review', model: 'fake', due: 'ROLLUP_AI is off' });
    assert.ok(line.includes('Scheduled drafting is off.'));
    assert.ok(line.includes('ROLLUP_AI'));
    assert.equal(line.includes('-'), false);
  });
});

describe('rail helpers', () => {
  test('the ISO week is the one the mock shows', () => {
    assert.equal(isoWeek(new Date(2026, 9, 23)), 43);
    assert.equal(isoWeek(new Date(2027, 0, 1)), 53);
  });

  test('"to fix" counts late items and items with no end date', () => {
    const draft = { entry_date: '2026-10-23', items: [
      { pct: 100, end_date: '2026-10-20' },
      { pct: 50, end_date: '2026-10-20' },
      { pct: 20, start_date: '2026-10-01' },
      { pct: 0 },
    ] };
    assert.equal(luFixCount(draft), 2);
    assert.equal(luFixCount(null), 0);
  });
});

describe('one page header', () => {
  test('pageSub writes the lede in row 2 and the actions and hint in row 3', async () => {
    const src = `
      const nodes = { 'page-sub': { innerHTML: '' }, 'page-acts': { innerHTML: '' } };
      const $ = (id) => nodes[id] || null;
      ${extractConst(html, 'esc')}
      ${extractFunction(html, 'pageSub')}
      export { pageSub, nodes };
    `;
    const { pageSub, nodes } = await importTemp(src);
    pageSub('Read only.', 'One hint.', '<div class="page-actions">Run</div>');
    assert.equal(nodes['page-sub'].innerHTML, '<p class="lede">Read only.</p>');
    assert.equal(nodes['page-acts'].innerHTML, '<div class="page-actions">Run</div><p class="meta">One hint.</p>');
    pageSub('<b>x</b>', '', '');
    assert.equal(nodes['page-sub'].innerHTML, '<p class="lede">&lt;b&gt;x&lt;/b&gt;</p>');
    pageSub('', '', '');
    assert.equal(nodes['page-sub'].innerHTML, '');
    assert.equal(nodes['page-acts'].innerHTML, '');
    const hsrc = `
      const nodes = { 'page-title': { innerHTML: '' }, 'page-sub': { innerHTML: '' }, 'page-acts': { innerHTML: '' } };
      const $ = (id) => nodes[id] || null;
      const pageEyebrow = () => '';
      ${extractConst(html, 'esc')}
      ${extractFunction(html, 'pageSub')}
      ${extractFunction(html, 'head')}
      export { head, nodes };
    `;
    const h = await importTemp(hsrc);
    assert.equal(h.head('T', 'Claim', 'Hint', '<div>x</div>'), '');
    assert.equal(h.nodes['page-title'].innerHTML, '<h1 title="T">T</h1>');
    assert.equal(h.nodes['page-sub'].innerHTML, '<p class="lede">Claim</p>');
    assert.equal(h.nodes['page-acts'].innerHTML, '<div>x</div><p class="meta">Hint</p>');
    assert.match(html, /<div class="page-row">[\s\S]*<div class="page-sub"[\s\S]*<div class="page-acts"/);
  });
});
