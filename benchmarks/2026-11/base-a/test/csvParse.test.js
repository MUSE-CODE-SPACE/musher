import { test } from 'node:test'; import assert from 'node:assert/strict';
import { csvParse } from '../src/csvParse.js';
test('simple', () => { assert.deepEqual(csvParse('a,b\n1,2\n'), [['a','b'],['1','2']]); });
test('quoted comma', () => { assert.deepEqual(csvParse('"x,y",z'), [['x,y','z']]); });
test('escaped quote', () => { assert.deepEqual(csvParse('"he said ""hi""",ok'), [['he said "hi"','ok']]); });
test('newline in quotes', () => { assert.deepEqual(csvParse('"line1\nline2",b\nc,d'), [['line1\nline2','b'],['c','d']]); });
test('empty fields', () => { assert.deepEqual(csvParse('a,,c'), [['a','','c']]); });
