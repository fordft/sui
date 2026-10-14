const test = require('node:test');
const assert = require('node:assert/strict');
const { parse, inline } = require('../markdown');

test('blocks: paragraphs keep line breaks, headings cap at three levels', () => {
  assert.deepEqual(parse('one\ntwo\n\nthree'), [{ type: 'p', text: 'one\ntwo' }, { type: 'p', text: 'three' }]);
  assert.deepEqual(parse('# A\n## B\n#### C').map(b => b.level), [1, 2, 3]);
  assert.deepEqual(parse('before\n# Head\nafter').map(b => b.type), ['p', 'h', 'p']);
});

test('lists: bullets, numbers, and indented continuation lines', () => {
  assert.deepEqual(parse('- a\n- b\n  more\n* c'), [{ type: 'ul', items: ['a', 'b\nmore', 'c'] }]);
  assert.deepEqual(parse('1. x\n2) y'), [{ type: 'ol', items: ['x', 'y'] }]);
  assert.deepEqual(parse('- a\n1. b').map(b => b.type), ['ul', 'ol']);
});

test('quotes and rules', () => {
  assert.deepEqual(parse('> q1\n> q2'), [{ type: 'quote', text: 'q1\nq2' }]);
  assert.deepEqual(parse('---\n***').map(b => b.type), ['hr', 'hr']);
});

test('fenced code keeps its text verbatim and survives streaming', () => {
  assert.deepEqual(parse('```rust\nfn main() {}\n```'), [{ type: 'code', lang: 'rust', text: 'fn main() {}', closed: true }]);
  const open = parse('text\n```js\nconst a = 1;\n# not a heading\n- not a list');
  assert.equal(open[1].type, 'code');
  assert.equal(open[1].closed, false);
  assert.equal(open[1].text, 'const a = 1;\n# not a heading\n- not a list');
  assert.equal(parse('~~~\nx\n~~~')[0].type, 'code');
  assert.equal(parse('````\n```\ninner\n```\n````')[0].text, '```\ninner\n```');
});

test('inline: code, bold, italic, links', () => {
  assert.deepEqual(inline('a `b` **c** *d* [e](f)'), [
    { t: 'text', v: 'a ' }, { t: 'code', v: 'b' }, { t: 'text', v: ' ' }, { t: 'b', v: 'c' },
    { t: 'text', v: ' ' }, { t: 'i', v: 'd' }, { t: 'text', v: ' ' }, { t: 'link', v: 'e', href: 'f' }
  ]);
  assert.deepEqual(inline('`` a`b ``'), [{ t: 'code', v: 'a`b' }]);
});

test('inline: markers that are not formatting stay literal', () => {
  for (const text of ['2 * 3 * 4', 'snake*case*name', '**unfinished', '`open', '[x](', '* alone', '**  **']) {
    assert.ok(inline(text).every(t => t.t === 'text' || t.t === 'code'), text);
  }
  assert.equal(inline('**unfinished').map(t => t.v).join(''), '**unfinished');
});

test('html in model output is never interpreted', () => {
  const tokens = inline('<img src=x onerror=alert(1)> <script>x</script>');
  assert.ok(tokens.every(t => t.t === 'text'));
  assert.match(tokens.map(t => t.v).join(''), /<img src=x/);
});
