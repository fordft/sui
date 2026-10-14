const test = require('node:test');
const assert = require('node:assert/strict');
const pet = require('../pet');

test('sanitize falls back to defaults for anything unknown', () => {
  assert.deepEqual(pet.sanitize(undefined), pet.DEFAULTS);
  assert.deepEqual(pet.sanitize('nope'), pet.DEFAULTS);
  const odd = pet.sanitize({ color: 'constructor', accessory: '__proto__', size: 'xl', on: 0, follow: 'x', name: '' });
  assert.equal(odd.color, 'azure');
  assert.equal(odd.accessory, 'none');
  assert.equal(odd.size, 'm');
  assert.equal(odd.name, 'Sui');
  assert.equal(odd.on, true, 'only an explicit false hides the companion');
  assert.equal(odd.follow, true);
});

test('sanitize keeps valid choices and bounds the name', () => {
  const p = pet.sanitize({ on: false, color: 'grape', accessory: 'crown', size: 'l', follow: false, name: '  Bloop\u0007\n  ' });
  assert.deepEqual(p, { on: false, color: 'grape', accessory: 'crown', size: 'l', name: 'Bloop', follow: false });
  assert.equal(pet.sanitize({ name: 'x'.repeat(80) }).name.length, 20);
});

test('every color is a complete palette and every size leaves room to hop', () => {
  for (const [id, c] of Object.entries(pet.COLORS)) {
    for (const key of ['light', 'body', 'deep', 'rim']) assert.match(c[key], /^#[0-9a-f]{6}$/i, `${id}.${key}`);
  }
  for (const [id, s] of Object.entries(pet.SIZES)) assert.ok(s.lane >= s.w * 0.84 + 20, `${id} lane fits body + hop`);
});

test('a hop crouches, leaps, and lands where it started', () => {
  const start = pet.hopPose(0);
  const end = pet.hopPose(0.999);
  assert.equal(start.lift, 0);
  assert.ok(Math.abs(start.sx - end.sx) < 0.02 && Math.abs(start.sy - end.sy) < 0.02, 'hops chain without a pop');
  assert.ok(start.sy < 0.85 && start.sx > 1.1, 'crouch squashes');
  const peak = pet.hopPose(0.47);
  assert.ok(Math.abs(peak.lift - 1) < 0.01);
  assert.ok(peak.sy > 1 && peak.sx < 1, 'airborne stretches');
  for (let ph = 0; ph <= 1; ph += 0.01) {
    const p = pet.hopPose(ph);
    assert.ok(p.lift >= 0 && p.lift <= 1.0001);
    assert.ok(p.sx * p.sy > 0.85 && p.sx * p.sy < 1.2, `volume stays plausible at ${ph}`);
  }
});

test('the slime only travels while airborne', () => {
  assert.equal(pet.hopTravel(0), 0);
  assert.equal(pet.hopTravel(0.1), 0);
  assert.equal(pet.hopTravel(0.9), 1);
  let last = 0;
  for (let ph = 0; ph <= 1; ph += 0.02) { const t = pet.hopTravel(ph); assert.ok(t >= last); last = t; }
});

test('there is a distinct phrase list for the working bubble', () => {
  assert.ok(pet.PHRASES.length >= 4);
  assert.equal(new Set(pet.PHRASES).size, pet.PHRASES.length);
});
