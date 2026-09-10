function formatDateInput(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`
}

export function broadcastDateInput(timestamp: number, gridStartHour: number): string {
  const date = new Date(timestamp)
  if (date.getHours() < gridStartHour) date.setDate(date.getDate() - 1)
  return formatDateInput(date)
}
