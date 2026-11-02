import { test } from 'node:test'; import assert from 'node:assert/strict';
import { wrapText } from '../src/wrapText.js';
test('wrap', () => { assert.deepEqual(wrapText('the quick brown fox jumps', 10), ['the quick', 'brown fox', 'jumps']); });
test('long word', () => { assert.deepEqual(wrapText('a supercalifragilistic b', 5), ['a', 'supercalifragilistic', 'b']); });
test('extra spaces', () => { assert.deepEqual(wrapText('  hello   world  ', 20), ['hello world']); });
test('empty', () => { assert.deepEqual(wrapText('', 10), []); });
