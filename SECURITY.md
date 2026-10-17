# セキュリティポリシー

*English summary: Only the latest release is supported. Please report vulnerabilities privately via GitHub's "Report a vulnerability" (private security advisory) on this repository, not as a public issue. Include the version, steps to reproduce, and impact. This is a one-person project; I respond on a best-effort basis, aiming within about 2 weeks.*

## サポート対象のバージョン

最新のリリースのみが対象です。古いバージョンをお使いの場合は、最新版へアップデートしてください。

## 脆弱性の報告方法

セキュリティ上の問題を見つけたら、**公開の Issue には書かず**、GitHub の非公開の報告機能をご利用ください。

1. このリポジトリの「Security」タブを開く
2. 「Report a vulnerability」を選ぶ（非公開のセキュリティアドバイザリが作成されます）

連絡用のメールアドレスは公開していません。

## 報告に含めてほしいこと

- Sagasu のバージョン（設定の一番下に表示）と Windows のバージョン
- 問題の内容と、想定される影響
- 再現手順（可能であれば）
- 公開の予定がある場合は、その時期のご希望

## 対応の目安

個人で開発しているため、対応はベストエフォートです。2 週間程度を目安に、まず返信するよう努めます。修正後、ご希望があればお名前を明記してお礼をお伝えします。

## 対象範囲の目安

特に次の点に関する報告を歓迎します。

- **自動アップデート**：署名の検証を回避してしまうような問題
- **ファイルの移動・整理機能**：意図しないファイルの移動や削除、想定外の場所への書き込みにつながる問題
- **ローカルのインデックス**：検索用データ（`%APPDATA%\jp.sagasu.app`）の扱いによる情報漏えいなど

Sagasu は利用状況の送信（テレメトリ）を行わず、通信はアップデートの確認（GitHub Releases）のみです。
