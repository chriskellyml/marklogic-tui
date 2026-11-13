import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, it, vi } from 'vitest';
import { DocumentViewer } from './DocumentViewer';
import type { DocumentDetail } from './documents-api';

vi.mock('@uiw/react-codemirror', () => ({ default: ({ value }: { value: string }) => <pre data-testid="content">{value}</pre> }));

const detail = (format: 'xml' | 'json', content: string): DocumentDetail => ({
  uri: '/document', format, content, collections: [], metadata: {}, permissions: [], quality: null, timestamp: null, properties: '', warnings: [],
});

it.each([
  ['xml', '<root><a>value</a><b/></root>', '<root>\n  <a>value</a>\n  <b/>\n</root>'],
  ['json', '{"a":{"b":1}}', '{\n  "a": {\n    "b": 1\n  }\n}'],
] as const)('shows formatted %s by default and switches to and from raw', async (format, raw, formatted) => {
  render(<DocumentViewer document={detail(format, raw)} />);
  expect(screen.getByTestId('content')).toHaveTextContent(formatted, { normalizeWhitespace: false });
  await userEvent.click(screen.getByRole('button', { name: 'Raw' }));
  expect(screen.getByTestId('content')).toHaveTextContent(raw, { normalizeWhitespace: false });
  expect(screen.getByRole('button', { name: 'Raw' })).toHaveAttribute('aria-pressed', 'true');
  await userEvent.click(screen.getByRole('button', { name: 'Raw' }));
  expect(screen.getByTestId('content')).toHaveTextContent(formatted, { normalizeWhitespace: false });
});

it('leaves significant XML mixed content intact', () => {
  const raw = '<p>Hello <b>world</b>!</p>';
  render(<DocumentViewer document={detail('xml', raw)} />);
  expect(screen.getByTestId('content')).toHaveTextContent(raw, { normalizeWhitespace: false });
});
