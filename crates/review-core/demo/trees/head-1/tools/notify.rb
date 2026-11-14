#!/usr/bin/env ruby
# frozen_string_literal: true

# Posts the accounts that do not reconcile to a chat webhook, for the nightly job.

require "json"
require "net/http"
require "optparse"

module Ledger
  Mismatch = Struct.new(:account, :expected, :actual, keyword_init: true) do
    def difference = actual - expected

    def to_s
      format("%<account>-20s expected %<expected>d, found %<actual>d", to_h)
    end
  end

  class Notifier
    DEFAULT_CHANNEL = "#ledger"

    attr_reader :webhook

    def initialize(webhook:, channel: DEFAULT_CHANNEL, dry_run: false)
      @webhook = URI(webhook)
      @channel = channel
      @dry_run = dry_run
    end

    def notify(mismatches)
      return if mismatches.empty?

      text = ["*#{mismatches.size} accounts do not reconcile*", *mismatches.map(&:to_s)].join("\n")
      return puts(text) if @dry_run

      response = Net::HTTP.post(webhook, { channel: @channel, text: text }.to_json,
                                "Content-Type" => "application/json")
      raise "webhook failed: #{response.code}" unless response.is_a?(Net::HTTPSuccess)
    end
  end
end

if $PROGRAM_NAME == __FILE__
  options = { dry_run: false }
  OptionParser.new do |parser|
    parser.banner = "usage: notify.rb [--dry-run] [WEBHOOK] < mismatches.json"
    parser.on("--dry-run", "print the message instead of posting it") { options[:dry_run] = true }
  end.parse!

  rows = JSON.parse($stdin.read, symbolize_names: true)
  mismatches = rows.map { |row| Ledger::Mismatch.new(**row) }.sort_by { |m| -m.difference.abs }
  webhook = ARGV.fetch(0, ENV.fetch("LEDGER_WEBHOOK", "https://chat.example.com/hooks/ledger"))
  Ledger::Notifier.new(webhook: webhook, **options).notify(mismatches)
end
