import { test } from 'node:test'; import assert from 'node:assert/strict';
import { parseDuration } from '../src/parseDuration.js';
test('basic', () => { assert.equal(parseDuration('1h30m'), 5400); assert.equal(parseDuration('45s'), 45); assert.equal(parseDuration('2h'), 7200); assert.equal(parseDuration('1h2m3s'), 3723); });
test('invalid', () => { for (const x of ['', 'abc', '1x', 'h1', '1h1h']) assert.throws(() => parseDuration(x)); });
