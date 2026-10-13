{{- define "refhir.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "refhir.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "refhir.labels" -}}
app.kubernetes.io/name: {{ include "refhir.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
The refhir container and pod volumes. Called with a dict: root (the chart context), role
("" for the single-process install, "writer" or "replica" in cluster mode), and data ("pvc" or
"empty").
*/}}
{{- define "refhir.pod" -}}
{{- $root := .root -}}
containers:
  - name: refhir
    image: "{{ $root.Values.image.repository }}:{{ $root.Values.image.tag }}"
    imagePullPolicy: {{ $root.Values.image.pullPolicy }}
    env:
      - name: REFHIR_CONFIG
        value: /etc/refhir/refhir.toml
      - name: REFHIR_PACKAGE_CACHE
        value: /data/package-cache
      {{- if eq .role "writer" }}
      - name: REFHIR_ROLE
        value: standby
      {{- else if eq .role "replica" }}
      - name: REFHIR_ROLE
        value: replica
      - name: REFHIR_PRIMARY_URL
        value: {{ default (printf "http://%s-primary:%v" (include "refhir.fullname" $root) $root.Values.service.port) $root.Values.cluster.primaryUrl | quote }}
      {{- end }}
      {{- range $key, $value := $root.Values.secretEnv }}
      {{- if $value }}
      - name: {{ $key }}
        valueFrom:
          secretKeyRef:
            name: {{ include "refhir.fullname" $root }}-secrets
            key: {{ $key }}
      {{- end }}
      {{- end }}
    ports:
      - name: http
        containerPort: 8080
    # Ready means "send traffic here": 200 from the live writer and from a replica that is
    # keeping up, 503 from a standby and from a replica that has fallen behind.
    readinessProbe:
      httpGet:
        path: /healthz
        port: http
      periodSeconds: 2
      failureThreshold: 2
    # A standby is up and answering long before it is ready, so liveness only checks the port.
    livenessProbe:
      tcpSocket:
        port: http
      periodSeconds: 10
      failureThreshold: 6
    startupProbe:
      tcpSocket:
        port: http
      periodSeconds: 5
      failureThreshold: 120
    volumeMounts:
      - name: config
        mountPath: /etc/refhir
        readOnly: true
      - name: data
        mountPath: /data
      {{- if $root.Values.database.tlsSecret }}
      - name: postgres-tls
        mountPath: /etc/refhir-pg-tls
        readOnly: true
      {{- end }}
    resources:
      {{- toYaml $root.Values.resources | nindent 6 }}
volumes:
  - name: config
    configMap:
      name: {{ include "refhir.fullname" $root }}
  - name: data
    {{- if and (eq .data "pvc") $root.Values.persistence.enabled }}
    persistentVolumeClaim:
      claimName: {{ include "refhir.fullname" $root }}
    {{- else }}
    emptyDir: {}
    {{- end }}
  {{- if $root.Values.database.tlsSecret }}
  - name: postgres-tls
    secret:
      secretName: {{ $root.Values.database.tlsSecret }}
  {{- end }}
{{- end }}

{{- define "refhir.checkCluster" -}}
{{- if and .Values.cluster.enabled (ne .Values.database.kind "postgres") -}}
{{- fail "cluster.enabled needs database.kind: postgres (SQLite cannot be shared between pods)" -}}
{{- end -}}
{{- end }}
